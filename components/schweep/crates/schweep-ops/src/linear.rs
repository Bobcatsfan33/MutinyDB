//! The linear operators: filter and project (`ARCHITECTURE.md` §5.3; `docs/SEMANTICS.md` S-24,
//! S-25).
//!
//! ## What "linear" buys, and why it is the whole of C1
//!
//! An operator `f` is linear when `f(a + b) = f(a) + f(b)` over Z-sets. Filter and project both
//! are: filter keeps an entry or drops it based on its row alone, and project rewrites an entry's
//! row and keeps its weight. Neither decision looks at any other entry, so neither looks at any
//! other epoch.
//!
//! That is exactly why the incremental form is trivial — **the output delta is the operator
//! applied to the input delta** — and why these operators need no state. Summed over a history:
//!
//! ```text
//! Σ f(Δ₁ … Δₙ)  =  f(Σ Δ₁ … Δₙ)  =  f(integral)  =  what the oracle recomputes
//! ```
//!
//! The left-hand side is what the circuit maintains, the right-hand side is what the oracle
//! computes from scratch, and I-1 is the claim that they are the same bytes. For linear operators
//! that claim is a one-line consequence of linearity — which is what makes C1 the right place to
//! prove the *machinery* around it before C2 introduces an operator where the equality is a real
//! theorem with three terms.
//!
//! ## Statelessness is a rule here, not an outcome
//!
//! §6 C1's pitfall: "resist adding any state to linear operators; if a linear operator seems to
//! need state, the design is wrong." Both operators below declare [`StateBound::Stateless`] and
//! report a [`state_size`] of zero, and the circuit checks the declaration against the report
//! after every step (I-9). Adding a field that survives a step would fail that check.
//!
//! [`state_size`]: crate::Operator::state_size

use schweep_plan::bind::{projection_schema, Naming, Scope};
use schweep_plan::eval::{eval, is_true};
use schweep_plan::plan::{Expr, Named};
use schweep_plan::type_of;
use schweep_zset::{DataType, Row, Schema, ZSetBatch};

use crate::error::{OpError, Result};
use crate::operator::{error_row, unary, Operator, StateBound, StepOutput};

/// `WHERE`: keep the entries whose predicate is TRUE, with their weights untouched (S-24).
#[derive(Debug, Clone)]
pub struct Filter {
    input_schema: Schema,
    predicate: Expr,
}

impl Filter {
    /// Build a filter, checking that the predicate is Boolean in this scope (S-17).
    ///
    /// `naming` says how columns are written here — qualified `alias.column` before a GROUP BY,
    /// unqualified declared names after one (S-10, S-27). The operator does not guess: guessing
    /// would be a second implementation of the scoping rule, and the caller building the circuit
    /// already knows the answer.
    pub fn new(input_schema: Schema, naming: Naming, predicate: Expr) -> Result<Filter> {
        let scope = Scope::new(&input_schema, naming);
        match type_of(&predicate, scope)? {
            DataType::Boolean => {}
            found => {
                return Err(OpError::PredicateNotBoolean {
                    op: "filter",
                    found,
                })
            }
        }
        Ok(Filter {
            input_schema,
            predicate,
        })
    }

    #[must_use]
    pub fn predicate(&self) -> &Expr {
        &self.predicate
    }
}

impl Operator for Filter {
    fn name(&self) -> &'static str {
        "filter"
    }

    fn arity(&self) -> usize {
        1
    }

    /// Filtering does not change the shape of a row, only which rows there are.
    fn output_schema(&self) -> &Schema {
        &self.input_schema
    }

    fn state_bound(&self) -> StateBound {
        StateBound::Stateless
    }

    /// A linear operator was handed no store, and §6 C1 says it must stay that way.
    fn backend_count(&self) -> usize {
        0
    }

    fn state_size(&self) -> usize {
        0
    }

    fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput> {
        let input = unary("filter", inputs)?;
        check_schema("filter", &self.input_schema, input)?;

        let mut kept = Vec::new();
        let mut errors = Vec::new();
        for (row, weight) in input.entries()? {
            // The weight is carried through untouched, and its sign is never consulted: an entry
            // at -1 is filtered by exactly the test that filters an entry at +1 (I-5, S-24).
            match is_true(&self.predicate, &row, &self.input_schema) {
                Ok(true) => kept.push((row, weight)),
                Ok(false) => {}
                // A row whose predicate raises has no truth value, so it cannot be kept and cannot
                // be dropped silently: it is dropped and its error recorded at the row's weight
                // (S-22a, S-22b).
                Err(e) if e.is_evaluation_error() => errors.push((error_row(&e), weight)),
                Err(e) => return Err(OpError::Plan(e)),
            }
        }
        StepOutput::new(
            ZSetBatch::from_entries(self.input_schema.clone(), kept)?,
            errors,
        )
    }
}

/// `SELECT`: evaluate the declared expressions per entry, keeping the entry's weight (S-25).
///
/// Because a projection can drop distinguishing columns, two input rows can become one output
/// row; the result is consolidated, summing their weights. That is plain multiset semantics —
/// `SELECT` without `DISTINCT` preserves duplicates — and it is why a projection of two rows can
/// be one row at weight 2.
#[derive(Debug, Clone)]
pub struct Project {
    input_schema: Schema,
    output_schema: Schema,
    items: Vec<Named<Expr>>,
}

impl Project {
    /// Build a projection, computing its output schema through the shared binder helper.
    ///
    /// The schema comes from [`projection_schema`] rather than being derived here, because S-11
    /// implemented twice is S-11 that can disagree with itself — and a disagreement about an
    /// answer's schema is a disagreement about the answer (S-8, D-14).
    pub fn new(input_schema: Schema, naming: Naming, items: Vec<Named<Expr>>) -> Result<Project> {
        let output_schema = {
            let scope = Scope::new(&input_schema, naming);
            projection_schema(scope, &items)?
        };
        Ok(Project {
            input_schema,
            output_schema,
            items,
        })
    }

    #[must_use]
    pub fn items(&self) -> &[Named<Expr>] {
        &self.items
    }
}

impl Operator for Project {
    fn name(&self) -> &'static str {
        "project"
    }

    fn arity(&self) -> usize {
        1
    }

    fn output_schema(&self) -> &Schema {
        &self.output_schema
    }

    fn state_bound(&self) -> StateBound {
        StateBound::Stateless
    }

    /// A linear operator was handed no store, and §6 C1 says it must stay that way.
    fn backend_count(&self) -> usize {
        0
    }

    fn state_size(&self) -> usize {
        0
    }

    fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput> {
        let input = unary("project", inputs)?;
        check_schema("project", &self.input_schema, input)?;

        let mut projected = Vec::with_capacity(input.len());
        let mut errors = Vec::new();
        for (row, weight) in input.entries()? {
            let mut values = Vec::with_capacity(self.items.len());
            let mut raised = false;
            for item in &self.items {
                match eval(&item.value, &row, &self.input_schema) {
                    Ok(value) => values.push(value),
                    // A row missing a value in any output column cannot be emitted (S-22a).
                    Err(e) if e.is_evaluation_error() => {
                        errors.push((error_row(&e), weight));
                        raised = true;
                        break;
                    }
                    Err(e) => return Err(OpError::Plan(e)),
                }
            }
            if !raised {
                projected.push((Row::new(values), weight));
            }
        }
        // Consolidate: distinct input rows that projected to the same output row merge, and their
        // weights add (S-25). This is also where a +1 and a -1 for the same projected row cancel.
        let batch = ZSetBatch::from_entries(self.output_schema.clone(), projected)?;
        StepOutput::new(batch.consolidate()?, errors)
    }
}

fn check_schema(op: &'static str, expected: &Schema, input: &ZSetBatch) -> Result<()> {
    if input.schema() != expected {
        return Err(OpError::InputSchemaMismatch {
            op,
            expected: expected.to_string(),
            found: input.schema().to_string(),
        });
    }
    Ok(())
}
