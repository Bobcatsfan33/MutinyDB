//! The hand-built circuit API (`ARCHITECTURE.md` §6 C1) and the guarantees the step scheduler
//! makes.
//!
//! The differential harness proves the circuit *agrees with the oracle*. These tests prove the
//! things the harness cannot see because the oracle has no opinion about them: what the builder
//! refuses, what happens to the epoch counter when a step fails, and what a circuit holds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_circuit::{CircuitBuilder, CircuitError};
use schweep_ops::{Filter, Project};
use schweep_plan::bind::Naming;
use schweep_plan::plan::{BinOp, Named};
use schweep_plan::Expr;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

pub(crate) fn input_schema() -> Schema {
    Schema::new(vec![
        Field::nullable("t.a", DataType::Int64),
        Field::nullable("t.b", DataType::Int64),
    ])
    .unwrap()
}

pub(crate) fn row(a: Option<i64>, b: Option<i64>) -> Row {
    Row::new(vec![
        a.map_or(Value::Null, Value::Int),
        b.map_or(Value::Null, Value::Int),
    ])
}

pub(crate) fn epoch(entries: Vec<(Row, i64)>) -> EpochDeltas {
    let mut d = EpochDeltas::new();
    d.extend("t", entries);
    d
}

/// A scan, a filter, and a projection, wired by hand and stepped over several epochs.
#[test]
fn a_hand_built_circuit_maintains_its_answer_from_deltas() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let filter = builder
        .add(
            Box::new(
                Filter::new(
                    input_schema(),
                    Naming::Qualified,
                    Expr::binary(BinOp::Gt, Expr::column("t.a"), Expr::int(0)),
                )
                .unwrap(),
            ),
            vec![source],
        )
        .unwrap();
    let project = builder
        .add(
            Box::new(
                Project::new(
                    input_schema(),
                    Naming::Qualified,
                    vec![Named::new("a", Expr::column("t.a"))],
                )
                .unwrap(),
            ),
            vec![filter],
        )
        .unwrap();
    let mut circuit = builder.build(project).unwrap();

    assert_eq!(circuit.epoch(), 0);
    assert!(circuit.answer().unwrap().is_empty());

    // Epoch 1: two rows pass the filter and project to the same output row, so they merge (S-25).
    circuit
        .step(&epoch(vec![
            (row(Some(1), Some(10)), 1),
            (row(Some(1), Some(20)), 1),
            (row(Some(-5), Some(30)), 1),
        ]))
        .unwrap();
    assert_eq!(circuit.epoch(), 1);
    assert_eq!(circuit.answer().unwrap().render(), "(a: Int64)\n(1) => 2\n");

    // Epoch 2: a retraction of one of them. The answer drops to weight 1 without recomputing.
    circuit
        .step(&epoch(vec![(row(Some(1), Some(10)), -1)]))
        .unwrap();
    assert_eq!(circuit.answer().unwrap().render(), "(a: Int64)\n(1) => 1\n");

    // Epoch 3: retract the other, and the row leaves entirely — no zero-weight tombstone.
    circuit
        .step(&epoch(vec![(row(Some(1), Some(20)), -1)]))
        .unwrap();
    assert!(circuit.answer().unwrap().is_empty());
    assert_eq!(circuit.result_store().unwrap().len(), 0);
}

/// A row inserted and retracted within one epoch nets to nothing.
#[test]
fn same_epoch_churn_leaves_no_trace() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let mut circuit = builder.build(source).unwrap();

    circuit
        .step(&epoch(vec![
            (row(Some(1), Some(1)), 2),
            (row(Some(1), Some(1)), -2),
            (row(Some(2), Some(2)), 1),
        ]))
        .unwrap();
    assert_eq!(
        circuit.answer().unwrap().render(),
        "(t.a: Int64, t.b: Int64)\n(2, 2) => 1\n"
    );
}

/// An empty epoch still seals, and the answer does not move (S-6, I-3).
#[test]
fn an_empty_epoch_advances_the_clock_and_nothing_else() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let mut circuit = builder.build(source).unwrap();

    circuit
        .step(&epoch(vec![(row(Some(1), Some(1)), 1)]))
        .unwrap();
    let before = circuit.answer().unwrap();
    circuit.step(&EpochDeltas::new()).unwrap();
    assert_eq!(circuit.epoch(), 2);
    assert_eq!(circuit.answer().unwrap(), before);
}

/// A circuit only sees the tables it was wired to.
#[test]
fn deltas_for_a_table_this_circuit_does_not_read_are_ignored() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let mut circuit = builder.build(source).unwrap();

    let mut deltas = epoch(vec![(row(Some(1), Some(1)), 1)]);
    deltas.push("some_other_table", Row::new(vec![Value::Int(9)]), 1);
    circuit.step(&deltas).unwrap();

    assert_eq!(
        circuit.answer().unwrap().render(),
        "(t.a: Int64, t.b: Int64)\n(1, 1) => 1\n"
    );
}

/// The builder refuses a forward reference, which is what makes index order a topological order
/// and the schedule deterministic (I-2).
#[test]
fn the_builder_refuses_wiring_that_is_not_in_dependency_order() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let filter = Filter::new(
        input_schema(),
        Naming::Qualified,
        Expr::is_not_null(Expr::column("t.a")),
    )
    .unwrap();
    // Node 1 cannot take input from node 7, which does not exist yet and never will.
    let err = builder
        .add(Box::new(filter), vec![schweep_circuit::NodeId::from(7)])
        .unwrap_err();
    assert!(
        matches!(err, CircuitError::NodeOutOfOrder { .. }),
        "expected NodeOutOfOrder, got {err}"
    );
    let _ = source;
}

/// Arity is checked at wiring time, not discovered at step time.
#[test]
fn the_builder_refuses_an_operator_wired_to_the_wrong_number_of_inputs() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let filter = Filter::new(
        input_schema(),
        Naming::Qualified,
        Expr::is_not_null(Expr::column("t.a")),
    )
    .unwrap();
    let err = builder
        .add(Box::new(filter), vec![source, source])
        .unwrap_err();
    assert!(
        matches!(
            err,
            CircuitError::WiringArity {
                op: "filter",
                expected: 1,
                found: 2
            }
        ),
        "expected WiringArity, got {err}"
    );
}

#[test]
fn the_builder_refuses_a_duplicate_source_and_an_empty_circuit() {
    let mut builder = CircuitBuilder::new();
    builder.source("t", "t", input_schema()).unwrap();
    assert!(matches!(
        builder.source("t", "t", input_schema()).unwrap_err(),
        CircuitError::DuplicateSource(_)
    ));

    let empty = CircuitBuilder::new();
    assert!(matches!(
        empty.build(schweep_circuit::NodeId::from(0)).unwrap_err(),
        CircuitError::EmptyCircuit
    ));
}

/// A predicate that is not Boolean is refused when the operator is built, not when data arrives
/// (S-17). A badly-typed circuit never gets the chance to answer anything.
#[test]
fn a_non_boolean_predicate_is_refused_at_construction() {
    let err = Filter::new(input_schema(), Naming::Qualified, Expr::column("t.a")).unwrap_err();
    assert!(
        err.to_string().contains("Boolean"),
        "the refusal must name the requirement: {err}"
    );
}

/// **An evaluation error seals its epoch and becomes the answer** (S-22, D-16, I-3).
///
/// This test replaces C1's `an_evaluation_error_aborts_the_step_without_advancing_the_epoch`, which
/// asserted the opposite and was **wrong** under the rule D-16 settled. Refusing to advance the
/// epoch was the I-3 violation in disguise: the epoch's other changes would be dropped, and the next
/// epoch would land on contents that never absorbed them, leaving the answer a mixture of epoch N−1
/// and epoch N+1. The epoch now seals; only the *answer* is an error.
///
/// And the error is a property of the contents, so it lasts exactly as long as the offending row.
#[test]
fn an_evaluation_error_seals_its_epoch_and_lasts_while_the_row_does() {
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let project = builder
        .add(
            Box::new(
                Project::new(
                    input_schema(),
                    Naming::Qualified,
                    vec![Named::new(
                        "q",
                        Expr::binary(BinOp::Div, Expr::column("t.a"), Expr::column("t.b")),
                    )],
                )
                .unwrap(),
            ),
            vec![source],
        )
        .unwrap();
    let mut circuit = builder.build(project).unwrap();

    circuit
        .step(&epoch(vec![(row(Some(6), Some(2)), 1)]))
        .unwrap();
    assert_eq!(circuit.answer().unwrap().render(), "(q: Int64)\n(3) => 1\n");

    // An epoch carrying a division by zero *and* a good row. The epoch seals, both are processed,
    // and the answer is the error.
    circuit
        .step(&epoch(vec![
            (row(Some(1), Some(0)), 1),
            (row(Some(8), Some(4)), 1),
        ]))
        .unwrap();
    assert_eq!(circuit.epoch(), 2, "the epoch seals (S-22, I-3)");
    let err = circuit.answer().unwrap_err();
    assert_eq!(
        err.to_string(),
        "division by zero in / (S-21)",
        "the message must be exactly the oracle's, with nothing added (I-1)"
    );

    // An unrelated later epoch does not clear it: the offending row is still there.
    circuit
        .step(&epoch(vec![(row(Some(9), Some(3)), 1)]))
        .unwrap();
    assert!(
        circuit.answer().is_err(),
        "the error is a property of the contents"
    );

    // Retract the offending row and the answer returns — including everything the erroring epochs
    // carried alongside it, which is what sealing them rather than dropping them bought.
    circuit
        .step(&epoch(vec![(row(Some(1), Some(0)), -1)]))
        .unwrap();
    assert_eq!(
        circuit.answer().unwrap().render(),
        "(q: Int64)\n(2) => 1\n(3) => 2\n",
        "6/2 = 3, 8/4 = 2, 9/3 = 3"
    );
    assert!(
        circuit.error_store().unwrap().is_empty(),
        "retracting the row retracted its error by the same arithmetic (S-22b, I-5)"
    );
}

/// The state fingerprint reports the wiring, the declarations, and the store — and reports the
/// same bytes for the same history (I-2).
#[test]
fn the_state_fingerprint_is_stable_and_reports_what_is_held() {
    let build_and_run = || {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let filter = builder
            .add(
                Box::new(
                    Filter::new(
                        input_schema(),
                        Naming::Qualified,
                        Expr::is_not_null(Expr::column("t.a")),
                    )
                    .unwrap(),
                ),
                vec![source],
            )
            .unwrap();
        let mut circuit = builder.build(filter).unwrap();
        circuit
            .step(&epoch(vec![
                (row(Some(1), Some(1)), 1),
                (row(None, Some(2)), 1),
            ]))
            .unwrap();
        circuit.state_fingerprint().unwrap()
    };

    let a = build_and_run();
    let b = build_and_run();
    assert_eq!(a, b, "the same history must fingerprint identically");

    assert!(a.contains("circuit @ epoch 1"));
    assert!(a.contains("node 0 source table=t"));
    assert!(
        a.contains("state_bound=stateless") && a.contains("state_size=0"),
        "linear operators must report no state:\n{a}"
    );
    assert!(
        a.contains("result store holds 1 row(s)"),
        "the null row was filtered out, leaving one:\n{a}"
    );
}

// ---------------------------------------------------------------------------------------------
// I-9: state accounting. These use purpose-built operators, because the point is to check what
// the *runtime* does when a declaration and reality disagree — and no real operator disagrees.
// ---------------------------------------------------------------------------------------------

mod accounting {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{epoch, input_schema, row};
    use schweep_circuit::{CircuitBuilder, CircuitError};
    use schweep_ops::{OpError, Operator, StateBound, StepOutput};
    use schweep_zset::{Schema, ZSetBatch};

    /// Declares a bound and then holds whatever it is told to hold. The point of the exercise.
    #[derive(Debug)]
    struct Hoarder {
        schema: Schema,
        bound: StateBound,
        arity: usize,
        held: usize,
        /// Entries retained per entry that arrives. `1` is honest; more than 1 outgrows the input.
        per_entry: usize,
    }

    impl Operator for Hoarder {
        fn name(&self) -> &'static str {
            "hoarder"
        }
        fn arity(&self) -> usize {
            self.arity
        }
        fn output_schema(&self) -> &Schema {
            &self.schema
        }
        fn state_bound(&self) -> StateBound {
            self.bound
        }
        fn state_size(&self) -> usize {
            self.held
        }
        fn step(&mut self, inputs: &[&ZSetBatch]) -> Result<StepOutput, OpError> {
            let first = inputs.first().copied().ok_or(OpError::Arity {
                op: "hoarder",
                expected: 1,
                found: 0,
            })?;
            self.held += first.len() * self.per_entry;
            StepOutput::infallible(first.clone())
        }
    }

    fn hoarder(bound: StateBound, arity: usize, per_entry: usize) -> Hoarder {
        Hoarder {
            schema: input_schema(),
            bound,
            arity,
            held: 0,
            per_entry,
        }
    }

    fn one_input() -> StateBound {
        StateBound::ProportionalToInputs {
            inputs: &["only"],
            factor: 1,
            constant: 0,
        }
    }

    /// An operator whose state grows faster than its input is caught, with the budget named.
    ///
    /// This is the shape of the bug I-9 exists to prevent: a join that stored the cross product
    /// would hold |A|·|B| entries against a budget of |A|+|B|.
    #[test]
    fn state_growing_faster_than_its_input_is_caught() {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let node = builder
            .add(Box::new(hoarder(one_input(), 1, 3)), vec![source])
            .unwrap();
        let mut circuit = builder.build(node).unwrap();

        let err = circuit
            .step(&epoch(vec![
                (row(Some(1), Some(1)), 1),
                (row(Some(2), Some(2)), 1),
            ]))
            .unwrap_err();
        match err {
            CircuitError::StateBoundViolated {
                op, actual, budget, ..
            } => {
                assert_eq!(op, "hoarder");
                assert_eq!(actual, 6, "3 entries kept per entry, 2 entries in");
                assert_eq!(budget, 2, "the budget is the entries handed to it");
            }
            other => panic!("expected StateBoundViolated, got {other}"),
        }
    }

    /// An operator that keeps one entry per entry it is given sits inside its budget.
    #[test]
    fn state_proportional_to_its_input_is_accepted() {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let node = builder
            .add(Box::new(hoarder(one_input(), 1, 1)), vec![source])
            .unwrap();
        let mut circuit = builder.build(node).unwrap();
        circuit
            .step(&epoch(vec![
                (row(Some(1), Some(1)), 1),
                (row(Some(2), Some(2)), 1),
            ]))
            .unwrap();
        circuit
            .step(&epoch(vec![(row(Some(3), Some(3)), 1)]))
            .unwrap();
        assert_eq!(circuit.epoch(), 2);
    }

    /// A declaration of `Stateless` is checked against zero, not against a budget.
    #[test]
    fn a_stateless_declaration_that_holds_anything_is_caught() {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let node = builder
            .add(Box::new(hoarder(StateBound::Stateless, 1, 1)), vec![source])
            .unwrap();
        let mut circuit = builder.build(node).unwrap();

        let err = circuit
            .step(&epoch(vec![(row(Some(1), Some(1)), 1)]))
            .unwrap_err();
        assert!(
            matches!(
                err,
                CircuitError::StateBoundViolated {
                    actual: 1,
                    budget: 0,
                    ..
                }
            ),
            "a stateless operator has a budget of zero, got {err}"
        );
    }

    /// A declaration naming a different number of inputs than the operator takes is refused at
    /// wiring time: a declaration that does not describe the operator cannot be checked.
    #[test]
    fn a_declaration_that_does_not_match_the_arity_is_refused() {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let bound = StateBound::ProportionalToInputs {
            inputs: &["left", "right"],
            factor: 1,
            constant: 0,
        };
        let err = builder
            .add(Box::new(hoarder(bound, 1, 1)), vec![source])
            .unwrap_err();
        assert!(
            matches!(
                err,
                CircuitError::StateDeclarationArityMismatch {
                    declared: 2,
                    arity: 1,
                    ..
                }
            ),
            "expected StateDeclarationArityMismatch, got {err}"
        );
    }

    /// `Unbounded` is refused through the single-query door, always (I-9).
    ///
    /// C6 built the registry that can admit it, and this did not change: admission is a property of a
    /// *registration*, and `CircuitBuilder` compiles one query with nobody to sign for it. The
    /// admitting door is `Circuit::attach` — see
    /// `unbounded_state_is_refused_by_default_and_admissible_on_request`.
    #[test]
    fn an_unbounded_declaration_is_never_admissible_through_the_builder() {
        let mut builder = CircuitBuilder::new();
        let source = builder.source("t", "t", input_schema()).unwrap();
        let bound = StateBound::Unbounded {
            reason: "aggregation over an unbounded key space",
        };
        let err = builder
            .add(Box::new(hoarder(bound, 1, 1)), vec![source])
            .unwrap_err();
        assert!(
            matches!(err, CircuitError::UnboundedStateNotAdmissible { .. }),
            "expected UnboundedStateNotAdmissible, got {err}"
        );
    }
}

/// An operator that declares unbounded state and holds some, so the I-9 admission has something to
/// admit.
///
/// **No v1 operator declares `Unbounded`** — the join, the aggregate and the distinct all keep state
/// proportional to their input and say so. The admission mechanism exists because I-9 requires it and
/// because C2's state checker deferred it to "when C6's registry can admit it", so the thing being
/// tested here is the mechanism, with a probe standing in for a future operator that needs it.
#[derive(Debug)]
struct UnboundedProbe {
    schema: Schema,
    held: usize,
}

impl schweep_ops::Operator for UnboundedProbe {
    fn name(&self) -> &'static str {
        "unbounded-probe"
    }

    fn arity(&self) -> usize {
        1
    }

    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn state_bound(&self) -> schweep_ops::StateBound {
        schweep_ops::StateBound::Unbounded {
            reason: "a probe: it remembers every entry it has ever seen",
        }
    }

    fn state_size(&self) -> usize {
        self.held
    }

    fn step(
        &mut self,
        inputs: &[&schweep_zset::ZSetBatch],
    ) -> schweep_ops::Result<schweep_ops::StepOutput> {
        let input = inputs.first().copied().ok_or(schweep_ops::OpError::Arity {
            op: "unbounded-probe",
            expected: 1,
            found: 0,
        })?;
        // Grows without bound on purpose, and faster than its input, so a budget check would fail it.
        self.held += input.len() * 8 + 1;
        schweep_ops::StepOutput::infallible(input.clone())
    }
}

/// Unbounded state is refused by default and accepted only with an explicit admission (I-9).
#[test]
fn unbounded_state_is_refused_by_default_and_admissible_on_request() {
    let probe = || {
        Box::new(UnboundedProbe {
            schema: input_schema(),
            held: 0,
        })
    };

    // The single-query door never admits anything: admission is a property of a *registration*.
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let refused = builder.add(probe(), vec![source]);
    assert!(
        matches!(
            refused,
            Err(CircuitError::UnboundedStateNotAdmissible {
                op: "unbounded-probe",
                ..
            })
        ),
        "{refused:?}"
    );

    // The shared-dataflow door refuses it too, unless the caller admits it.
    let mut builder = CircuitBuilder::new();
    let source = builder.source("t", "t", input_schema()).unwrap();
    let passthrough = builder
        .add(
            Box::new(Filter::new(input_schema(), Naming::Qualified, Expr::boolean(true)).unwrap()),
            vec![source],
        )
        .unwrap();
    let mut circuit = builder.build(passthrough).unwrap();

    let refused = circuit.attach(probe(), vec![source], false);
    assert!(
        matches!(
            refused,
            Err(CircuitError::UnboundedStateNotAdmissible { .. })
        ),
        "{refused:?}"
    );

    let admitted = circuit.attach(probe(), vec![source], true).unwrap();
    assert!(
        circuit.admitted_unbounded().contains(&admitted),
        "the admission is recorded against the node, where the state check will look for it"
    );

    // And the admitted node is exempt from the *budget* — it has none — while everything else is
    // still checked. Without the exemption this step would fail the I-9 accounting.
    circuit
        .step(&epoch(vec![
            (row(Some(1), Some(1)), 1),
            (row(Some(2), None), 1),
        ]))
        .unwrap();
    let fingerprint = circuit.state_fingerprint().unwrap();
    assert!(
        fingerprint.contains("budget=admitted-unbounded"),
        "an admitted operator's state is still reported, with no budget to pretend about:\n{fingerprint}"
    );
    assert!(
        fingerprint.contains("state_size=17"),
        "and its size is visible: 2 entries -> 17 held\n{fingerprint}"
    );
}
