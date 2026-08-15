//! The `Operator` trait and the state declarations I-9 requires (`ARCHITECTURE.md` §5.3).

use std::fmt;

use schweep_plan::PlanError;
use schweep_zset::{DataType, Field, Row, Schema, Value, ZSetBatch};

use crate::error::{OpError, Result};

/// The column name of the error stream.
pub const ERROR_COLUMN: &str = "error";

/// The schema of every operator's error stream: one non-null `Utf8` column holding the message.
///
/// An error's identity is its message (S-22b), so a stream of messages is all the answer needs: the
/// live errors are a Z-set like any other, weights and all, and the least message is simply the
/// first row of its canonical form (S-22c).
pub fn error_schema() -> Result<Schema> {
    Ok(Schema::new(vec![Field::not_null(
        ERROR_COLUMN,
        DataType::Utf8,
    )])?)
}

/// One entry of the error stream: the message, at the weight of the row that raised it.
///
/// Carrying the *row's* weight is what makes S-22b work. Retracting the offending row retracts its
/// error by the same arithmetic that retracts any other row, so "the answer comes back when the data
/// leaves" needs no special path — I-5, applied to errors.
pub fn error_row(error: &PlanError) -> Row {
    Row::new(vec![Value::Str(error.to_string())])
}

/// What one step produces: the output delta, and the delta of the live-error set (S-22).
///
/// Two streams rather than a `Result` because an evaluation error is not a failure of the step — it
/// is part of the answer. The epoch seals either way (S-22, and I-3: an epoch that raised must not
/// be skipped, or the next one would land on contents that never absorbed it).
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub data: ZSetBatch,
    pub errors: ZSetBatch,
}

impl StepOutput {
    /// A step that cannot raise: the error delta is empty.
    pub fn infallible(data: ZSetBatch) -> Result<StepOutput> {
        Ok(StepOutput {
            errors: ZSetBatch::empty(error_schema()?)?,
            data,
        })
    }

    /// A step that may have recorded live errors.
    pub fn new(data: ZSetBatch, error_entries: Vec<(Row, i64)>) -> Result<StepOutput> {
        Ok(StepOutput {
            errors: ZSetBatch::from_entries(error_schema()?, error_entries)?,
            data,
        })
    }
}

/// What an operator promises to remember between steps (I-9).
///
/// The declaration is the *contract*; [`Operator::state_size`] reports what is actually held, and
/// the runtime checks one against the other. An operator that exceeds its declaration is a bug,
/// not a tuning problem — which is only enforceable because the declaration exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateBound {
    /// Nothing is remembered between steps.
    ///
    /// This is where the linear operators live — filter, map, project — and §6 C1 says they stay
    /// here: "resist adding any state to linear operators; if a linear operator seems to need
    /// state, the design is wrong." A linear operator's output delta is the operator applied to
    /// the input delta, and computing that needs no memory of any earlier epoch.
    Stateless,

    /// State proportional to the accumulated size of the named inputs, within a declared constant
    /// factor.
    ///
    /// The join declares `["left", "right"]` with factor 1 — it keeps both sides' integrals indexed
    /// by key, so its state is O(|A| + |B|) with one entry per row.
    ///
    /// **Neither number may be tuned; both must be justified.** The `factor` exists because an
    /// operator can legitimately keep more than one entry per input row: an aggregate with four
    /// aggregate slots keeps a value multiset per slot, so its state is up to `1 + 4` entries per row.
    /// The `constant` exists because an operator can legitimately keep a fixed amount regardless of
    /// input: a grand total keeps one entry recording that it has emitted, and it keeps it even over an
    /// empty input (S-33). A reader should be able to *count* the entries each number claims.
    ///
    /// A wrong *complexity* still fails the check whatever these are — a cross-product join outgrows
    /// any fixed factor as soon as either side has a few rows.
    ProportionalToInputs {
        inputs: &'static [&'static str],
        factor: usize,
        constant: usize,
    },

    /// Unbounded by nature. Must be admitted explicitly at query registration (I-9); aggregation
    /// over an unbounded key space is the example §4 gives. Nothing declares this in C1.
    Unbounded { reason: &'static str },
}

impl fmt::Display for StateBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateBound::Stateless => f.write_str("stateless"),
            StateBound::ProportionalToInputs {
                inputs,
                factor,
                constant,
            } => {
                write!(
                    f,
                    "{constant} + {factor}x proportional to {}",
                    inputs.join(" + ")
                )
            }
            StateBound::Unbounded { reason } => write!(f, "unbounded ({reason})"),
        }
    }
}

/// A node in a circuit: consumes one epoch's input deltas, produces one epoch's output delta.
///
/// **Nothing in any implementation may inspect the sign of a weight** (I-5). A retraction takes
/// the same path as an insertion. If you find yourself writing `if weight < 0` here — outside
/// MIN/MAX multiset bookkeeping or the sign logic in `distinct`, neither of which exists yet —
/// you are re-deriving a bug.
pub trait Operator: fmt::Debug + Send {
    /// A short, stable name, used in state fingerprints and failure reports.
    fn name(&self) -> &'static str;

    /// How many input deltas [`Operator::step`] expects, in order.
    fn arity(&self) -> usize;

    /// The schema of the deltas this operator emits.
    fn output_schema(&self) -> &Schema;

    /// What this operator promises to remember between steps (I-9).
    fn state_bound(&self) -> StateBound;

    /// How many entries are actually retained between steps, right now.
    ///
    /// Entries rather than bytes: at C1 there is no backend to ask for a byte count, and the unit
    /// that matters for the declarations above is "how many rows am I holding". C8 replaces this
    /// with real accounting when `EXPLAIN STATE` arrives.
    fn state_size(&self) -> usize;

    /// Consume one epoch's input deltas and produce this epoch's output delta, plus the delta of
    /// the live-error set (S-22).
    ///
    /// `inputs` has exactly [`Operator::arity`] elements. An operator that is handed the wrong
    /// number returns [`OpError::Arity`] rather than assuming.
    ///
    /// A returned `Err` means the *step* could not be performed — a wiring mistake, a schema
    /// mismatch, a backend failure. An evaluation error over the data is **not** that: it goes into
    /// [`StepOutput::errors`], the offending row is dropped (S-22a), and the epoch seals normally.
    fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput>;

    /// How many state backends this operator was handed.
    ///
    /// `EXPLAIN STATE`'s byte estimate needs it, because a backend costs something even when empty: a
    /// join holds two stores and therefore two files' worth of overhead. Defaulted to one, which is the
    /// common case; a stateless operator overrides it to zero.
    fn backend_count(&self) -> usize {
        1
    }

    /// A deterministic rendering of whatever this operator remembers, for state fingerprints.
    ///
    /// The default is empty, which is the honest answer for a stateless operator. A stateful one
    /// overrides it, and must render in a fixed order (I-2) — the I-2 gate compares these strings,
    /// so an operator that rendered its state in hash order would make two identical runs look
    /// different.
    fn render_state(&self) -> Result<String> {
        Ok(String::new())
    }

    /// Serialise whatever this operator remembers, for a checkpoint (`docs/DURABILITY.md` C1).
    ///
    /// The default is empty, which is correct for a stateless operator and is why filter and project
    /// need no code here. A stateful one must round-trip: `restore(snapshot())` has to give an
    /// operator that behaves identically, because I-7's claim is byte-identical recovery.
    fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    /// Replace this operator's state with a snapshot. Replace, not merge (see `StateBackend`).
    fn restore(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// Fetch the single input of a unary operator, or report the arity mismatch.
pub(crate) fn unary<'a>(op: &'static str, inputs: &[&'a ZSetBatch]) -> Result<&'a ZSetBatch> {
    match inputs {
        [only] => Ok(only),
        _ => Err(OpError::Arity {
            op,
            expected: 1,
            found: inputs.len(),
        }),
    }
}
