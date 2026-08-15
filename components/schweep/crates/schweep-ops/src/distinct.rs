//! `DISTINCT` (`docs/SEMANTICS.md` S-34, D-17; `ARCHITECTURE.md` §5.3).
//!
//! §5.3 calls it "weight → sign function, stateful", and the second word is the interesting one.
//! The function itself is trivial: a row present at all appears exactly once. What is not trivial is
//! that "present at all" is a question about the row's **integral**, not about the delta:
//!
//! ```text
//! Δout[row] = sign(I[row] + Δ[row]) − sign(I[row])
//! ```
//!
//! An implementation that looked only at the delta would emit a spurious `+1` every time an
//! already-present row gained another copy, and would emit nothing when a row dropped from weight 3
//! to weight 1 — which is right — but also nothing when it dropped from 1 to 0, which is wrong. So
//! the operator keeps the integral of its input, behind `StateBackend` like every other operator's
//! state, with a declared bound (I-9).
//!
//! Nothing here reads the sign of a weight to decide *what kind* of change it is (I-5); it computes
//! a sign function of the total, which is a different thing and is the operator's definition.

use schweep_state::{Key, StateBackend, WriteBatch};
use schweep_zset::{Row, Schema, ZSetBatch};

use crate::error::{OpError, Result};
use crate::operator::{unary, Operator, StateBound, StepOutput};

/// The single input `DISTINCT` reads, named for its state declaration (I-9).
pub const DISTINCT_INPUTS: &[&str] = &["input"];

#[derive(Debug)]
pub struct Distinct {
    schema: Schema,
    /// The integral of the input, one entry per distinct row.
    state: Box<dyn StateBackend>,
}

impl Distinct {
    pub fn new(schema: Schema, state: Box<dyn StateBackend>) -> Distinct {
        Distinct { schema, state }
    }
}

fn row_key(row: &Row) -> Key {
    row.values().to_vec()
}

/// The sign function S-34 is defined by: present means weight above zero.
fn sign(weight: i64) -> i64 {
    i64::from(weight > 0)
}

impl Operator for Distinct {
    fn name(&self) -> &'static str {
        "distinct"
    }

    fn arity(&self) -> usize {
        1
    }

    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    /// One entry per distinct input row, and nothing else.
    fn state_bound(&self) -> StateBound {
        StateBound::ProportionalToInputs {
            inputs: DISTINCT_INPUTS,
            factor: 1,
            constant: 0,
        }
    }

    fn state_size(&self) -> usize {
        self.state.len()
    }

    fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput> {
        let input = unary("distinct", inputs)?;
        if input.schema() != &self.schema {
            return Err(OpError::InputSchemaMismatch {
                op: "distinct",
                expected: self.schema.to_string(),
                found: input.schema().to_string(),
            });
        }

        // Consolidate the delta first, so each row is considered once with its net change. Without
        // this a row appearing twice in one delta would be evaluated against a stale integral.
        let consolidated = input.consolidate()?;
        let mut out = Vec::new();
        let mut batch = WriteBatch::new();

        for (row, delta) in consolidated.entries()? {
            let key = row_key(&row);
            let before = self.state.get(&key)?.unwrap_or(0);
            let after = before
                .checked_add(delta)
                .ok_or(OpError::JoinWeightOverflow)?;
            let change = sign(after) - sign(before);
            if change != 0 {
                out.push((row, change));
            }
            batch.add(key, delta);
        }
        self.state.write(&batch)?;

        StepOutput::infallible(ZSetBatch::from_entries(self.schema.clone(), out)?)
    }

    fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.state.snapshot()?)
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        self.state.restore(bytes)?;
        Ok(())
    }

    fn render_state(&self) -> Result<String> {
        let mut out = String::new();
        for (key, weight) in self.state.iter_all()? {
            let rendered: Vec<String> = key.iter().map(ToString::to_string).collect();
            out.push_str(&format!(
                "    distinct: [{}] => {weight}\n",
                rendered.join(", ")
            ));
        }
        Ok(out)
    }
}
