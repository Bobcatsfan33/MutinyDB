//! What `schweepd`'s two bounds cost in bytes (I-10).
//!
//! Two constants in this crate steer resident memory, and neither may be folklore:
//!
//! - [`DEFAULT_SOURCE_QUEUE_BOUND`](crate::DEFAULT_SOURCE_QUEUE_BOUND) — batches a source may have
//!   queued and unsealed. Times the size of a batch, that is what the server holds on a client's behalf.
//! - [`SUBSCRIPTION_RING`](crate::SUBSCRIPTION_RING) — sealed epochs' deltas one query retains. Times the
//!   size of a delta, that is what the server holds on a subscriber's behalf.
//!
//! So the receipt each one needs is a **size**: bytes per queued batch, and bytes per retained delta.
//! Both are *deterministic* — a framed record's length and a rendered delta's length are pure functions of
//! their inputs, with no clock, no allocator and no machine in them. That is better than C8's page-cache
//! sweep could manage, and it means `testing/differential/tests/evidence.rs` recomputes these numbers and
//! compares them rather than trusting a file.
//!
//! Regenerate the artifact with:
//!
//! ```text
//! cargo run --release -p schweep-server --bin c9-costs > testing/evidence/c9-bounds.json
//! ```

use schweep_zset::{Row, Value};

use crate::engine::delta_between;
use crate::server::encode_batch;

/// Row counts measured, spanning a single-row batch to a wide one.
pub const SAMPLE_ROWS: [usize; 4] = [1, 10, 100, 1_000];

/// Padding widths measured: a narrow row, and one wide enough to dominate its own frame.
pub const SAMPLE_PADDINGS: [usize; 2] = [0, 480];

/// What one queued batch costs on the wire and therefore in the pending queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchCost {
    pub rows: usize,
    pub padding: usize,
    /// The framed `Append` record's length — the log's own encoding, which is also what the server holds.
    pub frame_bytes: usize,
}

/// What one epoch's delta costs for a subscriber, rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaCost {
    /// Rows that changed in the epoch.
    pub rows_changed: usize,
    /// The rendered delta's length — exactly the bytes `/subscribe` returns for that epoch.
    pub rendered_bytes: usize,
}

/// Every measurement, in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurements {
    pub batches: Vec<BatchCost>,
    pub deltas: Vec<DeltaCost>,
}

fn padded_row(id: i64, padding: usize) -> Row {
    if padding == 0 {
        return Row::new(vec![Value::Int(id), Value::Int(1)]);
    }
    Row::new(vec![Value::Int(id), Value::Str(format!("{id:padding$}"))])
}

/// Measure. Deterministic: same inputs, same bytes, on any machine.
#[must_use]
pub fn measure() -> Measurements {
    let mut batches = Vec::new();
    for padding in SAMPLE_PADDINGS {
        for rows in SAMPLE_ROWS {
            let entries: Vec<(Row, i64)> = (0..rows)
                .map(|id| (padded_row(id as i64, padding), 1i64))
                .collect();
            batches.push(BatchCost {
                rows,
                padding,
                frame_bytes: encode_batch("t", "token", &entries).len(),
            });
        }
    }

    let mut deltas = Vec::new();
    for rows in SAMPLE_ROWS {
        // A delta is the difference between two rendered answers. The worst realistic case is every row
        // of the answer changing value, which produces one addition *and* one retraction per row — so
        // this measures the expensive direction rather than the flattering one.
        let schema = "(k: Int64, s: Int64)\n";
        let mut before = schema.to_owned();
        let mut after = schema.to_owned();
        for id in 0..rows {
            before.push_str(&format!("({id}, 1) => 1\n"));
            after.push_str(&format!("({id}, 2) => 1\n"));
        }
        deltas.push(DeltaCost {
            rows_changed: rows,
            rendered_bytes: delta_between(&before, &after).len(),
        });
    }

    Measurements { batches, deltas }
}

impl Measurements {
    /// The largest batch measured, which is the one the queue bound must be justified against.
    #[must_use]
    pub fn widest_batch(&self) -> Option<&BatchCost> {
        self.batches.iter().max_by_key(|cost| cost.frame_bytes)
    }

    /// The largest delta measured, likewise for the ring.
    #[must_use]
    pub fn widest_delta(&self) -> Option<&DeltaCost> {
        self.deltas.iter().max_by_key(|cost| cost.rendered_bytes)
    }

    /// Bytes one source can hold at the bound, at the widest measured batch.
    #[must_use]
    pub fn queue_bytes_at_bound(&self, bound: usize) -> usize {
        self.widest_batch()
            .map_or(0, |cost| cost.frame_bytes.saturating_mul(bound))
    }

    /// Bytes one query's ring can hold when full, at the widest measured delta.
    #[must_use]
    pub fn ring_bytes_when_full(&self, ring: usize) -> usize {
        self.widest_delta()
            .map_or(0, |cost| cost.rendered_bytes.saturating_mul(ring))
    }

    /// The artifact, written by hand because the workspace has no serde and does not want one.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n  \"$comment\": [\n");
        for line in [
            "C9 · what schweepd's two bounds cost in bytes (I-10).",
            "Regenerate: cargo run --release -p schweep-server --bin c9-costs > testing/evidence/c9-bounds.json",
            "DETERMINISTIC: a framed record's length and a rendered delta's length are pure functions",
            "of their inputs, so testing/differential/tests/evidence.rs recomputes these and compares",
            "them. If this file drifts from the code, that test fails rather than the ledger quietly",
            "describing something else.",
        ] {
            out.push_str(&format!("    {},\n", quote(line)));
        }
        out.pop();
        out.pop();
        out.push_str("\n  ],\n");

        out.push_str("  \"queued_batch_bytes\": [\n");
        for (index, cost) in self.batches.iter().enumerate() {
            out.push_str(&format!(
                "    {{ \"rows\": {}, \"padding\": {}, \"frame_bytes\": {} }}{}\n",
                cost.rows,
                cost.padding,
                cost.frame_bytes,
                if index + 1 == self.batches.len() {
                    ""
                } else {
                    ","
                }
            ));
        }
        out.push_str("  ],\n");

        out.push_str("  \"retained_delta_bytes\": [\n");
        for (index, cost) in self.deltas.iter().enumerate() {
            out.push_str(&format!(
                "    {{ \"rows_changed\": {}, \"rendered_bytes\": {} }}{}\n",
                cost.rows_changed,
                cost.rendered_bytes,
                if index + 1 == self.deltas.len() {
                    ""
                } else {
                    ","
                }
            ));
        }
        out.push_str("  ],\n");

        out.push_str("  \"at_the_compiled_settings\": {\n");
        let widest_batch = self.widest_batch().map_or(0, |cost| cost.frame_bytes);
        let widest_delta = self.widest_delta().map_or(0, |cost| cost.rendered_bytes);
        let narrow_batch = self
            .batches
            .iter()
            .find(|cost| cost.rows == 100 && cost.padding == 0)
            .map_or(0, |cost| cost.frame_bytes);
        for (key, value) in [
            ("source_queue_bound", crate::DEFAULT_SOURCE_QUEUE_BOUND),
            ("source_queue_bytes", crate::DEFAULT_SOURCE_QUEUE_BYTES),
            ("widest_measured_batch_bytes", widest_batch),
            (
                "bytes_one_source_would_hold_at_the_count_bound_alone",
                self.queue_bytes_at_bound(crate::DEFAULT_SOURCE_QUEUE_BOUND),
            ),
            (
                "widest_batches_admitted_by_the_byte_bound",
                divide(crate::DEFAULT_SOURCE_QUEUE_BYTES, widest_batch),
            ),
            ("narrow_100_row_batch_bytes", narrow_batch),
            (
                "bytes_one_source_holds_at_the_count_bound_for_narrow_batches",
                narrow_batch.saturating_mul(crate::DEFAULT_SOURCE_QUEUE_BOUND),
            ),
            ("subscription_ring", crate::SUBSCRIPTION_RING),
            ("subscription_ring_bytes", crate::SUBSCRIPTION_RING_BYTES),
            ("widest_measured_delta_bytes", widest_delta),
            (
                "bytes_one_query_retains_at_the_count_bound",
                self.ring_bytes_when_full(crate::SUBSCRIPTION_RING),
            ),
            (
                "widest_deltas_admitted_by_the_byte_bound",
                divide(crate::SUBSCRIPTION_RING_BYTES, widest_delta),
            ),
        ] {
            out.push_str(&format!("    \"{key}\": {value},\n"));
        }
        out.pop();
        out.pop();
        out.push_str("\n  }\n}\n");
        out
    }
}

/// Integer division that says 0 rather than dividing by zero.
fn divide(total: usize, each: usize) -> usize {
    if each == 0 {
        return 0;
    }
    total / each
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_measurement_is_deterministic() {
        assert_eq!(measure(), measure());
        assert_eq!(measure().to_json(), measure().to_json());
    }

    #[test]
    fn a_batch_costs_more_when_its_rows_are_wider() {
        let measured = measure();
        let narrow = measured
            .batches
            .iter()
            .find(|cost| cost.rows == 100 && cost.padding == 0)
            .unwrap()
            .frame_bytes;
        let wide = measured
            .batches
            .iter()
            .find(|cost| cost.rows == 100 && cost.padding == 480)
            .unwrap()
            .frame_bytes;
        assert!(
            wide > narrow * 10,
            "a padded row must dominate its frame: {narrow} vs {wide}"
        );
    }
}
