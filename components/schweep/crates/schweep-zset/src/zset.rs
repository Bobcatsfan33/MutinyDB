//! Z-set batches over Arrow (`ARCHITECTURE.md` §5.2, D-2; `docs/SEMANTICS.md` S-4, S-8).
//!
//! A [`ZSetBatch`] is an Arrow `RecordBatch` plus an aligned `i64` weight column. Weight `+3`
//! means three copies of the row; `-1` means one copy is removed. There is no separate delete
//! or update machinery anywhere in Current: an update is a `-1` for the old row and a `+1` for
//! the new row in the same Z-set.
//!
//! ## The one thing to understand before changing this file
//!
//! **Nothing here looks at the sign of a weight.** [`ZSetBatch::add`] concatenates, [`negate`]
//! flips, [`consolidate`] sums and drops zeros. A retraction takes exactly the same code path as
//! an insertion, which is invariant I-5, and it is I-5 that the whole engine's handling of
//! deletion rests on. If you find yourself writing `if weight < 0` in this crate, stop: you are
//! re-deriving a bug.
//!
//! [`negate`]: ZSetBatch::negate
//! [`consolidate`]: ZSetBatch::consolidate
//!
//! ## Performance, honestly
//!
//! `consolidate` is C10's sort+merge implementation: columns are decoded once, rows are sorted in a
//! contiguous vector, and equal neighbours are merged in one linear pass. It still materialises a
//! row representation at the Arrow boundary; eliminating that last representation requires a measured
//! Arrow-native ordering that preserves S-7 exactly, not an unbenchmarked claim (I-10).

use std::fmt;
use std::sync::Arc;

use arrow_array::builder::{BooleanBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
};

use crate::error::{Result, ZSetError};
use crate::row::Row;
use crate::schema::{Field, Schema};
use crate::value::{DataType, Value};

/// A multiset of rows with integer weights, stored columnar.
#[derive(Clone, Debug)]
pub struct ZSetBatch {
    schema: Schema,
    batch: RecordBatch,
    weights: Int64Array,
}

impl ZSetBatch {
    /// The empty Z-set over `schema`: no entries, which is the additive identity.
    pub fn empty(schema: Schema) -> Result<ZSetBatch> {
        ZSetBatch::from_entries(schema, Vec::new())
    }

    /// Build from `(row, weight)` entries, validating every value against the schema.
    ///
    /// Entries are stored as given: duplicates are *not* merged and zero weights are *not*
    /// dropped. That is [`ZSetBatch::consolidate`]'s job, called deliberately, not a side effect
    /// of construction.
    pub fn from_entries(schema: Schema, entries: Vec<(Row, i64)>) -> Result<ZSetBatch> {
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.len());
        for (index, field) in schema.fields().iter().enumerate() {
            let mut column_values: Vec<&Value> = Vec::with_capacity(entries.len());
            for (row, _) in &entries {
                if row.len() != schema.len() {
                    return Err(ZSetError::ArityMismatch {
                        expected: schema.len(),
                        found: row.len(),
                    });
                }
                let value = row.get(index).ok_or(ZSetError::ArityMismatch {
                    expected: schema.len(),
                    found: row.len(),
                })?;
                schema.check_value(index, value)?;
                column_values.push(value);
            }
            columns.push(build_array(field, &column_values)?);
        }

        let weights: Vec<i64> = entries.iter().map(|(_, w)| *w).collect();
        let batch = RecordBatch::try_new(schema.to_arrow(), columns)?;
        Ok(ZSetBatch {
            schema,
            batch,
            weights: Int64Array::from(weights),
        })
    }

    /// Adopt an existing Arrow batch and weight column (the zero-copy door, D-2).
    ///
    /// The Arrow schema must match `schema` field for field, and the weight column must be the
    /// same length as the batch.
    pub fn from_arrow(
        schema: Schema,
        batch: RecordBatch,
        weights: Int64Array,
    ) -> Result<ZSetBatch> {
        let expected = schema.to_arrow();
        if batch.schema() != expected {
            return Err(ZSetError::SchemaMismatch {
                left: format!("{expected:?}"),
                right: format!("{:?}", batch.schema()),
            });
        }
        if weights.len() != batch.num_rows() {
            return Err(ZSetError::WeightLengthMismatch {
                weights: weights.len(),
                rows: batch.num_rows(),
            });
        }
        if weights.null_count() > 0 {
            return Err(ZSetError::Arrow(
                "weight column contains nulls; a weight is always an integer".to_owned(),
            ));
        }
        Ok(ZSetBatch {
            schema,
            batch,
            weights,
        })
    }

    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The Arrow batch holding the row data. Weights live alongside it, in [`ZSetBatch::weights`].
    #[must_use]
    pub fn record_batch(&self) -> &RecordBatch {
        &self.batch
    }

    /// The aligned `i64` weight column: `weights[i]` is the weight of batch row `i`.
    #[must_use]
    pub fn weights(&self) -> &Int64Array {
        &self.weights
    }

    /// Number of *entries*, not the number of distinct rows and not the total weight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batch.num_rows()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materialise `(row, weight)` entries in stored order (§5.2: "iteration by (row, weight)").
    pub fn entries(&self) -> Result<Vec<(Row, i64)>> {
        let n = self.len();
        let mut out = Vec::with_capacity(n);
        // Read column-major (one downcast per column, not per cell), assemble row-major.
        let mut columns: Vec<Vec<Value>> = Vec::with_capacity(self.schema.len());
        for (index, field) in self.schema.fields().iter().enumerate() {
            let array = self
                .batch
                .columns()
                .get(index)
                .ok_or_else(|| ZSetError::UnknownColumn(field.name.clone()))?;
            columns.push(read_column(field, array.as_ref(), n)?);
        }
        for i in 0..n {
            let mut values = Vec::with_capacity(columns.len());
            for column in &columns {
                let v = column.get(i).ok_or(ZSetError::WeightLengthMismatch {
                    weights: self.weights.len(),
                    rows: n,
                })?;
                values.push(v.clone());
            }
            let weight =
                self.weights
                    .values()
                    .get(i)
                    .copied()
                    .ok_or(ZSetError::WeightLengthMismatch {
                        weights: self.weights.len(),
                        rows: n,
                    })?;
            out.push((Row::new(values), weight));
        }
        Ok(out)
    }

    /// Z-set addition: multiset union with weight addition.
    ///
    /// This concatenates the two entry lists. Entries for equal rows are *not* merged here —
    /// merging is [`ZSetBatch::consolidate`], which is called at chosen points rather than on
    /// every addition. Consequently `a.add(b)` and `b.add(a)` differ in physical layout while
    /// being the same Z-set; equality of Z-sets is equality of canonical forms (S-8), and the
    /// property tests assert commutativity and associativity in exactly those terms.
    pub fn add(&self, other: &ZSetBatch) -> Result<ZSetBatch> {
        if self.schema != other.schema {
            return Err(ZSetError::SchemaMismatch {
                left: self.schema.to_string(),
                right: other.schema.to_string(),
            });
        }
        let mut entries = self.entries()?;
        entries.extend(other.entries()?);
        ZSetBatch::from_entries(self.schema.clone(), entries)
    }

    /// Negate every weight: the additive inverse, and the operation that turns an insertion into
    /// a retraction and back (I-5).
    ///
    /// Errors on `i64::MIN`, which has no negation. Saturating here would silently change a
    /// quantity, which is exactly the kind of lie D-11 forbids.
    pub fn negate(&self) -> Result<ZSetBatch> {
        let mut entries = self.entries()?;
        for (_, w) in &mut entries {
            *w = w.checked_neg().ok_or(ZSetError::WeightOverflow {
                while_doing: "negating a weight",
            })?;
        }
        ZSetBatch::from_entries(self.schema.clone(), entries)
    }

    /// Merge entries for equal rows by summing weights, drop zero-weight entries, and order the
    /// result by all columns in schema order (S-8).
    ///
    /// This is where "an insert and a delete cancel" physically happens. The output is in
    /// canonical form, so `consolidate` is idempotent — a property test pins that.
    ///
    /// Ordering comes from a stable total-order sort, never a hash map: iteration order must be a
    /// function of the data alone (I-2). Stability also preserves the input order of equal rows, so
    /// checked-overflow behaviour is unchanged from applying their weights in arrival order.
    pub fn consolidate(&self) -> Result<ZSetBatch> {
        let merged = self.consolidated_entries()?;
        ZSetBatch::from_entries(self.schema.clone(), merged)
    }

    /// The canonical form of this Z-set (S-8): consolidated, zero weights dropped, sorted.
    ///
    /// This is the value the differential harness compares (I-1).
    pub fn canonical(&self) -> Result<Canonical> {
        Ok(Canonical {
            schema: self.schema.clone(),
            entries: self.consolidated_entries()?,
        })
    }

    fn consolidated_entries(&self) -> Result<Vec<(Row, i64)>> {
        let mut entries = self.entries()?;
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));

        let mut merged: Vec<(Row, i64)> = Vec::with_capacity(entries.len());
        for (row, weight) in entries {
            if weight == 0 {
                continue;
            }
            let same_as_previous = merged.last().is_some_and(|(previous, _)| previous == &row);
            if same_as_previous {
                let remove = if let Some((_, previous_weight)) = merged.last_mut() {
                    *previous_weight =
                        previous_weight
                            .checked_add(weight)
                            .ok_or(ZSetError::WeightOverflow {
                                while_doing: "consolidating weights for equal rows",
                            })?;
                    *previous_weight == 0
                } else {
                    false
                };
                if remove {
                    merged.pop();
                }
            } else {
                merged.push((row, weight));
            }
        }
        Ok(merged)
    }
}

/// A Z-set in canonical form (S-8): consolidated, zero weights dropped, sorted by all columns in
/// schema order — together with its schema, because schema equality is part of answer equality.
///
/// Two answers are equal iff their canonical forms are equal. This type is what "byte for byte"
/// means at rungs 1–3 (I-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canonical {
    schema: Schema,
    entries: Vec<(Row, i64)>,
}

impl Canonical {
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    #[must_use]
    pub fn entries(&self) -> &[(Row, i64)] {
        &self.entries
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// A deterministic textual rendering, used for byte-level comparison and for failure
    /// messages that a human can read without a debugger.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.schema.to_string());
        out.push('\n');
        for (row, weight) in &self.entries {
            out.push_str(&format!("{row} => {weight}\n"));
        }
        out
    }
}

impl fmt::Display for Canonical {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

fn build_array(field: &Field, values: &[&Value]) -> Result<ArrayRef> {
    let mismatch = |found: DataType| ZSetError::ValueTypeMismatch {
        column: field.name.clone(),
        expected: field.data_type,
        found,
    };
    match field.data_type {
        DataType::Int64 => {
            let mut builder = Int64Builder::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Null => builder.append_null(),
                    Value::Int(i) => builder.append_value(*i),
                    other => return Err(mismatch(other.data_type().unwrap_or(DataType::Int64))),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Utf8 => {
            let mut builder = StringBuilder::new();
            for v in values {
                match v {
                    Value::Null => builder.append_null(),
                    Value::Str(s) => builder.append_value(s),
                    other => return Err(mismatch(other.data_type().unwrap_or(DataType::Utf8))),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Boolean => {
            let mut builder = BooleanBuilder::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Null => builder.append_null(),
                    Value::Bool(b) => builder.append_value(*b),
                    other => return Err(mismatch(other.data_type().unwrap_or(DataType::Boolean))),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Float64 => {
            let mut builder = Float64Builder::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Null => builder.append_null(),
                    Value::Float(x) => builder.append_value(*x),
                    other => return Err(mismatch(other.data_type().unwrap_or(DataType::Float64))),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
    }
}

fn read_column(field: &Field, array: &dyn Array, rows: usize) -> Result<Vec<Value>> {
    if array.len() != rows {
        return Err(ZSetError::WeightLengthMismatch {
            weights: rows,
            rows: array.len(),
        });
    }
    let downcast_failed = || ZSetError::ArrowDowncast {
        column: field.name.clone(),
        expected: field.data_type,
    };
    let mut out = Vec::with_capacity(rows);
    match field.data_type {
        DataType::Int64 => {
            let a = array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(downcast_failed)?;
            for i in 0..rows {
                out.push(if a.is_null(i) {
                    Value::Null
                } else {
                    Value::Int(a.value(i))
                });
            }
        }
        DataType::Utf8 => {
            let a = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(downcast_failed)?;
            for i in 0..rows {
                out.push(if a.is_null(i) {
                    Value::Null
                } else {
                    Value::Str(a.value(i).to_owned())
                });
            }
        }
        DataType::Boolean => {
            let a = array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(downcast_failed)?;
            for i in 0..rows {
                out.push(if a.is_null(i) {
                    Value::Null
                } else {
                    Value::Bool(a.value(i))
                });
            }
        }
        DataType::Float64 => {
            let a = array
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(downcast_failed)?;
            for i in 0..rows {
                out.push(if a.is_null(i) {
                    Value::Null
                } else {
                    Value::Float(a.value(i))
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn schema() -> Schema {
        Schema::new(vec![
            Field::nullable("k", DataType::Int64),
            Field::nullable("s", DataType::Utf8),
        ])
        .unwrap()
    }

    fn row(k: Option<i64>, s: Option<&str>) -> Row {
        Row::new(vec![
            k.map_or(Value::Null, Value::Int),
            s.map_or(Value::Null, |x| Value::Str(x.to_owned())),
        ])
    }

    fn zset(entries: Vec<(Row, i64)>) -> ZSetBatch {
        ZSetBatch::from_entries(schema(), entries).unwrap()
    }

    #[test]
    fn round_trips_entries_through_arrow() {
        let entries = vec![
            (row(Some(1), Some("a")), 3),
            (row(None, None), -2),
            (row(Some(-5), Some("")), 1),
        ];
        let z = zset(entries.clone());
        assert_eq!(z.len(), 3);
        assert_eq!(z.entries().unwrap(), entries);
        assert_eq!(z.record_batch().num_rows(), 3);
        assert_eq!(z.weights().len(), 3);
    }

    #[test]
    fn insert_and_delete_cancel_under_consolidate() {
        let z = zset(vec![
            (row(Some(1), Some("a")), 1),
            (row(Some(1), Some("a")), -1),
        ]);
        assert_eq!(z.len(), 2, "add does not merge; consolidate does");
        assert!(z.consolidate().unwrap().is_empty());
        assert!(z.canonical().unwrap().is_empty());
    }

    #[test]
    fn consolidate_sums_weights_and_sorts_nulls_first() {
        let z = zset(vec![
            (row(Some(2), Some("b")), 1),
            (row(None, Some("z")), 4),
            (row(Some(2), Some("b")), 2),
            (row(Some(1), Some("a")), 0),
        ]);
        let c = z.canonical().unwrap();
        assert_eq!(
            c.entries(),
            &[(row(None, Some("z")), 4), (row(Some(2), Some("b")), 3)]
        );
    }

    #[test]
    fn a_row_can_pass_through_zero_and_come_back() {
        // +1, -1, +5 for the same row: the running sum touches zero mid-fold and must not
        // strand the row out of the map.
        let r = row(Some(7), Some("q"));
        let z = zset(vec![(r.clone(), 1), (r.clone(), -1), (r.clone(), 5)]);
        assert_eq!(z.canonical().unwrap().entries(), &[(r, 5)]);
    }

    #[test]
    fn negate_flips_every_weight() {
        let z = zset(vec![(row(Some(1), None), 2), (row(Some(2), None), -3)]);
        let n = z.negate().unwrap();
        assert_eq!(
            n.entries().unwrap(),
            vec![(row(Some(1), None), -2), (row(Some(2), None), 3)]
        );
    }

    #[test]
    fn negate_refuses_i64_min_rather_than_saturating() {
        let z = zset(vec![(row(Some(1), None), i64::MIN)]);
        assert_eq!(
            z.negate().unwrap_err(),
            ZSetError::WeightOverflow {
                while_doing: "negating a weight"
            }
        );
    }

    #[test]
    fn consolidate_reports_weight_overflow_rather_than_wrapping() {
        let r = row(Some(1), None);
        let z = zset(vec![(r.clone(), i64::MAX), (r, 1)]);
        assert_eq!(
            z.consolidate().unwrap_err(),
            ZSetError::WeightOverflow {
                while_doing: "consolidating weights for equal rows"
            }
        );
    }

    #[test]
    fn add_refuses_mismatched_schemas() {
        let other_schema = Schema::new(vec![Field::nullable("k", DataType::Int64)]).unwrap();
        let a = zset(vec![]);
        let b = ZSetBatch::empty(other_schema).unwrap();
        assert!(matches!(a.add(&b), Err(ZSetError::SchemaMismatch { .. })));
    }

    #[test]
    fn from_entries_validates_values_against_the_schema() {
        let bad = ZSetBatch::from_entries(
            schema(),
            vec![(Row::new(vec![Value::Str("no".into()), Value::Null]), 1)],
        );
        assert!(matches!(bad, Err(ZSetError::ValueTypeMismatch { .. })));

        let short = ZSetBatch::from_entries(schema(), vec![(Row::new(vec![Value::Int(1)]), 1)]);
        assert!(matches!(short, Err(ZSetError::ArityMismatch { .. })));
    }

    #[test]
    fn from_arrow_checks_alignment_of_the_weight_column() {
        let z = zset(vec![(row(Some(1), None), 1)]);
        let err = ZSetBatch::from_arrow(
            schema(),
            z.record_batch().clone(),
            Int64Array::from(vec![1_i64, 2]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ZSetError::WeightLengthMismatch {
                weights: 2,
                rows: 1
            }
        );
    }

    #[test]
    fn from_arrow_refuses_null_weights() {
        let z = zset(vec![(row(Some(1), None), 1)]);
        let err = ZSetBatch::from_arrow(
            schema(),
            z.record_batch().clone(),
            Int64Array::from(vec![None::<i64>]),
        )
        .unwrap_err();
        assert!(matches!(err, ZSetError::Arrow(_)));
    }

    #[test]
    fn canonical_includes_the_schema_in_equality() {
        let a = zset(vec![(row(Some(1), Some("x")), 1)])
            .canonical()
            .unwrap();
        let other_schema = Schema::new(vec![
            Field::nullable("k2", DataType::Int64),
            Field::nullable("s", DataType::Utf8),
        ])
        .unwrap();
        let b = ZSetBatch::from_entries(other_schema, vec![(row(Some(1), Some("x")), 1)])
            .unwrap()
            .canonical()
            .unwrap();
        assert_ne!(a, b, "same rows, different schema, different answer (S-8)");
    }

    #[test]
    fn render_is_stable_and_readable() {
        let z = zset(vec![(row(Some(1), Some("a")), 2), (row(None, None), -1)]);
        assert_eq!(
            z.canonical().unwrap().render(),
            "(k: Int64, s: Utf8)\n(NULL, NULL) => -1\n(1, \"a\") => 2\n"
        );
    }
}
