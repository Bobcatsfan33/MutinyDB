//! The equi-join: the first **bilinear** operator (`ARCHITECTURE.md` §5.3, D-3; `docs/SEMANTICS.md`
//! S-26).
//!
//! §6 calls C2 "the hardest correctness class in the engine", and the reason is one line of algebra.
//! For a linear operator the incremental form is free. For a join it is not, and the derivation is
//! worth having in front of you before reading the code.
//!
//! ## The three-term rule, derived
//!
//! Let `A` and `B` be the two sides' integrals and `Out = A ⋈ B`. After an epoch delivers `ΔA` and
//! `ΔB`, the new integrals are `A' = A + ΔA` and `B' = B + ΔB`. Because join distributes over Z-set
//! addition on each side independently — that is what *bilinear* means:
//!
//! ```text
//! Out' = A' ⋈ B'
//!      = (A + ΔA) ⋈ (B + ΔB)
//!      = A ⋈ B  +  A ⋈ ΔB  +  ΔA ⋈ B  +  ΔA ⋈ ΔB
//!      = Out    +  A ⋈ ΔB  +  ΔA ⋈ B  +  ΔA ⋈ ΔB
//!
//! ΔOut = Out' - Out = ΔA ⋈ B  +  A ⋈ ΔB  +  ΔA ⋈ ΔB
//! ```
//!
//! **`A` and `B` in that formula are the integrals as they were *before* this epoch.** This is the
//! single most important sentence in the file. Probing the *updated* indexes would count this
//! epoch's own rows twice — once in `ΔA ⋈ B'` and again in `ΔA ⋈ ΔB` — and the answer would drift
//! upward on every epoch where both sides changed. So: **three probes first, integrate second**,
//! in that order, and [`Join::step`] does exactly that with the terms labelled.
//!
//! ## The term everybody forgets
//!
//! §6 C2's pitfall names it: `ΔA ⋈ ΔB`. It is invisible in every epoch where only one side changes,
//! which is most epochs, so an implementation missing it looks correct for a long time and then
//! quietly undercounts whenever both sides move together. It is also the only term that is *not* a
//! probe against stored state — it pairs this epoch's two deltas directly — which is why it is easy
//! to leave out of a design organised around indexes.
//!
//! `testing/differential/tests/c2_join.rs` carries a scenario that isolates it: one epoch, both
//! sides inserting a matching row, nothing in either index beforehand. Terms 1 and 2 both probe
//! empty indexes and contribute nothing, so the entire answer is term 3. Drop it and the test sees
//! an empty answer where the oracle sees a row.
//!
//! ## What the code may not do
//!
//! Nothing here inspects the sign of a weight (I-5). Weights are *multiplied*, and multiplication
//! does not care: a retraction on one side times an insertion on the other is a negative output
//! weight, which is precisely the retraction of the joined row. There is no deletion path.

use schweep_state::{Key, StateBackend, WriteBatch};
use schweep_zset::{Row, Schema, Value, ZSetBatch};

use crate::error::{OpError, Result};
use crate::operator::{Operator, StateBound, StepOutput};

/// Which side of a join a key column belongs to. Named so the state declaration can say what its
/// bound is proportional to (I-9).
pub const JOIN_INPUTS: &[&str] = &["left", "right"];

/// An INNER equi-join over one or more key-column pairs (S-26).
#[derive(Debug)]
pub struct Join {
    left_schema: Schema,
    right_schema: Schema,
    output_schema: Schema,
    /// Column indexes of the key, per side, in the order the pairs were declared.
    left_key: Vec<usize>,
    right_key: Vec<usize>,
    /// The two integrals, indexed by `[key values…, row values…]` so that "every row with this
    /// key" is a prefix scan (D-15).
    left_index: Box<dyn StateBackend>,
    right_index: Box<dyn StateBackend>,
}

impl Join {
    /// Build a join over the given key pairs.
    ///
    /// `on` names columns by their position in each side's schema. At least one pair is required —
    /// a join with none is a cross join, which is not in the rung-2 dialect (S-26).
    pub fn new(
        left_schema: Schema,
        right_schema: Schema,
        on: Vec<(usize, usize)>,
        left_index: Box<dyn StateBackend>,
        right_index: Box<dyn StateBackend>,
    ) -> Result<Join> {
        if on.is_empty() {
            return Err(OpError::Plan(
                schweep_plan::PlanError::CrossJoinNotSupported,
            ));
        }

        let mut left_key = Vec::with_capacity(on.len());
        let mut right_key = Vec::with_capacity(on.len());
        for (l, r) in &on {
            let lf = left_schema.field(*l).ok_or(OpError::JoinKeyOutOfRange {
                side: "left",
                index: *l,
            })?;
            let rf = right_schema.field(*r).ok_or(OpError::JoinKeyOutOfRange {
                side: "right",
                index: *r,
            })?;
            // An equi-join across types would need a coercion, and there are none (S-19).
            if lf.data_type != rf.data_type {
                return Err(OpError::Plan(schweep_plan::PlanError::TypeMismatch {
                    op: "join key =",
                    left: lf.data_type,
                    right: rf.data_type,
                }));
            }
            left_key.push(*l);
            right_key.push(*r);
        }

        // Output schema is the left schema followed by the right schema (S-26). Aliases are unique
        // within a query, so the names cannot collide; `Schema::new` refuses it if they somehow do.
        let mut fields = left_schema.fields().to_vec();
        fields.extend(right_schema.fields().iter().cloned());
        let output_schema = Schema::new(fields)?;

        Ok(Join {
            left_schema,
            right_schema,
            output_schema,
            left_key,
            right_key,
            left_index,
            right_index,
        })
    }

    /// Entries held across both indexes: the join's contribution to memory (I-9).
    #[must_use]
    pub fn state_entries(&self) -> usize {
        self.left_index.len() + self.right_index.len()
    }
}

/// Split a length-prefixed block off the front, returning it and the remainder.
fn split_prefixed(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let mut len_raw = [0u8; 4];
    len_raw.copy_from_slice(bytes.get(0..4).ok_or(OpError::CorruptJoinIndex)?);
    let len = u32::from_be_bytes(len_raw) as usize;
    let block = bytes.get(4..4 + len).ok_or(OpError::CorruptJoinIndex)?;
    let rest = bytes.get(4 + len..).ok_or(OpError::CorruptJoinIndex)?;
    Ok((block, rest))
}

/// The key values of a row, or `None` if any of them is null.
///
/// A row with a null key joins nothing: `NULL = NULL` is `NULL`, not `true`, so no comparison in
/// the pair can be TRUE (S-13, S-26). Returning `None` makes that a fact about the *row* rather
/// than something each caller has to remember, and it is why a null key is never used to probe —
/// the index does contain null-keyed rows, and a prefix scan on `[NULL]` would happily match them.
fn key_of(row: &Row, positions: &[usize]) -> Option<Vec<Value>> {
    let mut key = Vec::with_capacity(positions.len());
    for position in positions {
        match row.get(*position) {
            None | Some(Value::Null) => return None,
            Some(value) => key.push(value.clone()),
        }
    }
    Some(key)
}

/// The index key for a row: its join key, then the whole row, so a prefix scan by join key finds
/// every row under it and the full key stays unique per distinct row.
fn index_key(key: &[Value], row: &Row) -> Key {
    let mut full = Vec::with_capacity(key.len() + row.len());
    full.extend_from_slice(key);
    full.extend(row.values().iter().cloned());
    full
}

/// Recover the row from an index key, given how many leading values are the join key.
fn row_from_index_key(key: &Key, key_len: usize) -> Result<Row> {
    let values = key
        .get(key_len..)
        .ok_or(OpError::CorruptJoinIndex)?
        .to_vec();
    Ok(Row::new(values))
}

/// Concatenate a left and a right row and multiply their weights (S-26).
fn joined(left: &Row, left_weight: i64, right: &Row, right_weight: i64) -> Result<(Row, i64)> {
    // Multiplicities multiply: three copies of a left row against two matching right rows are six
    // copies of the joined row. Sign is irrelevant to multiplication, which is exactly why
    // retraction needs no special path here (I-5).
    let weight = left_weight
        .checked_mul(right_weight)
        .ok_or(OpError::JoinWeightOverflow)?;
    let mut values = left.values().to_vec();
    values.extend(right.values().iter().cloned());
    Ok((Row::new(values), weight))
}

impl Operator for Join {
    fn name(&self) -> &'static str {
        "join"
    }

    fn arity(&self) -> usize {
        2
    }

    fn output_schema(&self) -> &Schema {
        &self.output_schema
    }

    /// The join keeps both sides' integrals, so its state is O(|A| + |B|) — proportional to the
    /// rows that have flowed in, never to their product (I-9).
    fn state_bound(&self) -> StateBound {
        StateBound::ProportionalToInputs {
            inputs: JOIN_INPUTS,
            // One index entry per row on each side, and nothing else.
            factor: 1,
            constant: 0,
        }
    }

    fn state_size(&self) -> usize {
        self.state_entries()
    }

    /// Two: the join keeps an integral of each side (§6 C2).
    fn backend_count(&self) -> usize {
        2
    }

    /// Both indexes, in key order. `MemBackend` scans in the total order on values (S-7), so this
    /// is a function of the data alone (I-2).
    fn render_state(&self) -> Result<String> {
        self.render_indexes()
    }

    fn snapshot(&self) -> Result<Vec<u8>> {
        let left = self.left_index.snapshot()?;
        let right = self.right_index.snapshot()?;
        let mut out = Vec::with_capacity(left.len() + right.len() + 8);
        out.extend_from_slice(&(left.len() as u32).to_be_bytes());
        out.extend_from_slice(&left);
        out.extend_from_slice(&(right.len() as u32).to_be_bytes());
        out.extend_from_slice(&right);
        Ok(out)
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        let (left, rest) = split_prefixed(bytes)?;
        let (right, _) = split_prefixed(rest)?;
        self.left_index.restore(left)?;
        self.right_index.restore(right)?;
        Ok(())
    }

    /// `ΔOut = ΔA ⋈ B + A ⋈ ΔB + ΔA ⋈ ΔB`, three probes, no shortcuts (D-3, §5.3).
    fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput> {
        let (delta_left, delta_right) = match inputs {
            [l, r] => (*l, *r),
            _ => {
                return Err(OpError::Arity {
                    op: "join",
                    expected: 2,
                    found: inputs.len(),
                })
            }
        };
        if delta_left.schema() != &self.left_schema {
            return Err(OpError::InputSchemaMismatch {
                op: "join (left)",
                expected: self.left_schema.to_string(),
                found: delta_left.schema().to_string(),
            });
        }
        if delta_right.schema() != &self.right_schema {
            return Err(OpError::InputSchemaMismatch {
                op: "join (right)",
                expected: self.right_schema.to_string(),
                found: delta_right.schema().to_string(),
            });
        }

        let left_entries = delta_left.entries()?;
        let right_entries = delta_right.entries()?;
        let key_len = self.left_key.len();
        let mut out: Vec<(Row, i64)> = Vec::new();

        // ---- Term 1:  ΔA ⋈ B  — this epoch's left delta against the right integral AS IT WAS ----
        for (left_row, left_weight) in &left_entries {
            let Some(key) = key_of(left_row, &self.left_key) else {
                continue;
            };
            let mut failure = None;
            self.right_index
                .visit_prefix(&key, &mut |index_key, right_weight| {
                    let result = row_from_index_key(index_key, key_len).and_then(|right_row| {
                        joined(left_row, *left_weight, &right_row, right_weight)
                    });
                    match result {
                        Ok(entry) => out.push(entry),
                        Err(error) => {
                            failure = Some(error);
                            return false;
                        }
                    }
                    true
                })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }

        // ---- Term 2:  A ⋈ ΔB  — the left integral AS IT WAS against this epoch's right delta ----
        for (right_row, right_weight) in &right_entries {
            let Some(key) = key_of(right_row, &self.right_key) else {
                continue;
            };
            let mut failure = None;
            self.left_index
                .visit_prefix(&key, &mut |index_key, left_weight| {
                    let result = row_from_index_key(index_key, key_len).and_then(|left_row| {
                        joined(&left_row, left_weight, right_row, *right_weight)
                    });
                    match result {
                        Ok(entry) => out.push(entry),
                        Err(error) => {
                            failure = Some(error);
                            return false;
                        }
                    }
                    true
                })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }

        // ---- Term 3:  ΔA ⋈ ΔB  — the two deltas against each other -------------------------------
        //
        // The term §6 C2 says every implementer forgets. It is not a probe against state: neither
        // delta is in an index yet, and neither will be until below. Without it, an epoch in which
        // both sides gain a matching row produces nothing at all, because terms 1 and 2 each probe
        // an index that does not yet contain the other side's new row.
        for (left_row, left_weight) in &left_entries {
            let Some(left_key) = key_of(left_row, &self.left_key) else {
                continue;
            };
            for (right_row, right_weight) in &right_entries {
                let Some(right_key) = key_of(right_row, &self.right_key) else {
                    continue;
                };
                if left_key != right_key {
                    continue;
                }
                out.push(joined(left_row, *left_weight, right_row, *right_weight)?);
            }
        }

        // ---- Only now: integrate. ---------------------------------------------------------------
        //
        // Every probe above read the integrals as of the previous epoch, which is what the
        // derivation requires. Updating earlier would double-count this epoch's own rows.
        self.integrate(&left_entries, &right_entries)?;

        // The join raises nothing data-dependent: key comparison cannot fail, and a weight product
        // outside `i64` is an engine limit rather than an S-22 semantic error, so it stays a hard
        // failure of the step on both sides.
        let batch = ZSetBatch::from_entries(self.output_schema.clone(), out)?;
        StepOutput::infallible(batch.consolidate()?)
    }
}

impl Join {
    fn integrate(&mut self, left: &[(Row, i64)], right: &[(Row, i64)]) -> Result<()> {
        let mut left_batch = WriteBatch::new();
        for (row, weight) in left {
            // Null-keyed rows are stored like any other. They can never join, so storing them buys
            // nothing except that the index is exactly the side's integral — which keeps the state
            // bound honest and means a retraction is handled by the same path as an insertion.
            let key = key_of(row, &self.left_key).unwrap_or_else(|| {
                // A null-keyed row still needs a distinct index key. `Value::Null` sorts first and
                // is never a probe prefix, so it cannot be matched by accident.
                self.left_key.iter().map(|_| Value::Null).collect()
            });
            left_batch.add(index_key(&key, row), *weight);
        }
        let mut right_batch = WriteBatch::new();
        for (row, weight) in right {
            let key = key_of(row, &self.right_key)
                .unwrap_or_else(|| self.right_key.iter().map(|_| Value::Null).collect());
            right_batch.add(index_key(&key, row), *weight);
        }
        self.left_index.write(&left_batch)?;
        self.right_index.write(&right_batch)?;
        Ok(())
    }

    /// A deterministic rendering of both indexes, for state fingerprints (I-2).
    pub fn render_indexes(&self) -> Result<String> {
        let mut out = String::new();
        for (label, index) in [("left", &self.left_index), ("right", &self.right_index)] {
            for (key, weight) in index.iter_all()? {
                let rendered: Vec<String> = key.iter().map(ToString::to_string).collect();
                out.push_str(&format!(
                    "    {label}: [{}] => {weight}\n",
                    rendered.join(", ")
                ));
            }
        }
        Ok(out)
    }
}
