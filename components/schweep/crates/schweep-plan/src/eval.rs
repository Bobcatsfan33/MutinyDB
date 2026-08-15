//! Scalar expression evaluation with three-valued logic (`docs/SEMANTICS.md` S-13 … S-22).
//!
//! Every rule in this file is one of the numbered semantics rules, and the mapping is stated at
//! each site. If you are here to change behaviour, change `docs/SEMANTICS.md` first (§10).
//!
//! The style is the oracle's style: a column reference is resolved by scanning the schema for its
//! name, on every cell, every time. That is O(columns) per lookup and it is fine. This code is
//! read far more often than it is run, and the only property it must have is being obviously
//! right.

use schweep_zset::{Row, Schema, Value};

use crate::error::{PlanError, Result};
use crate::plan::{BinOp, Expr};

/// Evaluate `expr` against `row`, whose columns are described by `scope`.
///
/// Returns a [`Value`], which may be [`Value::Null`] — null is a *result*, not an error (S-13,
/// S-14). Errors are reserved for overflow and division by zero (S-20, S-21), which abort the
/// query for the epoch (S-22).
pub fn eval(expr: &Expr, row: &Row, scope: &Schema) -> Result<Value> {
    match expr {
        Expr::Column(name) => {
            let index = scope
                .index_of(name)
                .ok_or_else(|| PlanError::UnknownColumn {
                    name: name.clone(),
                    scope: scope.to_string(),
                })?;
            row.get(index)
                .cloned()
                .ok_or_else(|| PlanError::UnknownColumn {
                    name: name.clone(),
                    scope: scope.to_string(),
                })
        }

        Expr::Literal(v) => Ok(v.clone()),
        Expr::Null(_) => Ok(Value::Null),

        Expr::Binary { op, left, right } => {
            let l = eval(left, row, scope)?;
            let r = eval(right, row, scope)?;
            eval_binary(*op, &l, &r)
        }

        // Kleene NOT (S-15): NOT NULL is NULL.
        Expr::Not(inner) => Ok(match as_bool(&eval(inner, row, scope)?) {
            None => Value::Null,
            Some(b) => Value::Bool(!b),
        }),

        // Both operands are always evaluated (S-15): no short-circuit, so whether a query raises
        // an error never depends on evaluation order (I-2).
        Expr::And(left, right) => {
            let l = as_bool(&eval(left, row, scope)?);
            let r = as_bool(&eval(right, row, scope)?);
            Ok(kleene_and(l, r))
        }
        Expr::Or(left, right) => {
            let l = as_bool(&eval(left, row, scope)?);
            let r = as_bool(&eval(right, row, scope)?);
            Ok(kleene_or(l, r))
        }

        // Two-valued, always (S-16). The only way to observe a null as a boolean.
        Expr::IsNull(inner) => Ok(Value::Bool(eval(inner, row, scope)?.is_null())),
        Expr::IsNotNull(inner) => Ok(Value::Bool(!eval(inner, row, scope)?.is_null())),

        // CASE selects the first TRUE branch and evaluates only that branch's result (S-18).
        // Conditions before it *are* evaluated, so an error in an earlier condition is raised.
        Expr::Case { whens, otherwise } => {
            for (condition, result) in whens {
                if as_bool(&eval(condition, row, scope)?) == Some(true) {
                    return eval(result, row, scope);
                }
            }
            match otherwise {
                Some(else_expr) => eval(else_expr, row, scope),
                None => Ok(Value::Null),
            }
        }
    }
}

/// Evaluate a predicate and answer the only question `WHERE` and `HAVING` ask: is it TRUE?
///
/// `false` and `NULL` both mean "no" (S-17). Collapsing them here, once, is why no caller has to
/// remember to.
pub fn is_true(expr: &Expr, row: &Row, scope: &Schema) -> Result<bool> {
    Ok(as_bool(&eval(expr, row, scope)?) == Some(true))
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Kleene AND (S-15). The row that catches people: `FALSE AND NULL = FALSE`.
fn kleene_and(l: Option<bool>, r: Option<bool>) -> Value {
    match (l, r) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}

/// Kleene OR (S-15). The row that catches people: `TRUE OR NULL = TRUE`.
fn kleene_or(l: Option<bool>, r: Option<bool>) -> Value {
    match (l, r) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    // S-13 and S-14: any null operand yields null, and the operation is *not performed* — which
    // is why `NULL / 0` is NULL rather than a division-by-zero error.
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            let (a, b) = match (l, r) {
                (Value::Int(a), Value::Int(b)) => (*a, *b),
                // Unreachable after binding, which proves both sides are Int64 (S-19). Reported
                // rather than assumed, because an oracle that assumes is not an oracle.
                _ => {
                    return Err(PlanError::TypeMismatch {
                        op: op.name(),
                        left: l.data_type().unwrap_or(schweep_zset::DataType::Int64),
                        right: r.data_type().unwrap_or(schweep_zset::DataType::Int64),
                    })
                }
            };
            eval_arithmetic(op, a, b).map(Value::Int)
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            // Both operands are non-null and (after binding) the same type, so the total order
            // on values (S-7) coincides with the type's own order here. Null ordering never
            // enters: nulls were handled above.
            let ordering = l.cmp(r);
            Ok(Value::Bool(match op {
                BinOp::Eq => ordering.is_eq(),
                BinOp::Ne => ordering.is_ne(),
                BinOp::Lt => ordering.is_lt(),
                BinOp::Le => ordering.is_le(),
                BinOp::Gt => ordering.is_gt(),
                BinOp::Ge => ordering.is_ge(),
                // Arithmetic operators are matched in the arm above.
                _ => return Err(PlanError::ArithmeticOverflow { op: op.name() }),
            }))
        }
    }
}

/// Checked integer arithmetic (S-20, S-21). Nothing here wraps, saturates, or returns null.
fn eval_arithmetic(op: BinOp, a: i64, b: i64) -> Result<i64> {
    let overflow = || PlanError::ArithmeticOverflow { op: op.name() };
    match op {
        BinOp::Add => a.checked_add(b).ok_or_else(overflow),
        BinOp::Sub => a.checked_sub(b).ok_or_else(overflow),
        BinOp::Mul => a.checked_mul(b).ok_or_else(overflow),
        BinOp::Div => {
            if b == 0 {
                return Err(PlanError::DivisionByZero { op: op.name() });
            }
            // `checked_div` also catches i64::MIN / -1, which overflows (S-21).
            a.checked_div(b).ok_or_else(overflow)
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(PlanError::DivisionByZero { op: op.name() });
            }
            a.checked_rem(b).ok_or_else(overflow)
        }
        // Comparison operators are handled by the caller.
        _ => Err(overflow()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_zset::{DataType, Field};

    fn scope() -> Schema {
        Schema::new(vec![
            Field::nullable("t.i", DataType::Int64),
            Field::nullable("t.s", DataType::Utf8),
            Field::nullable("t.b", DataType::Boolean),
        ])
        .unwrap()
    }

    fn row(i: Option<i64>, s: Option<&str>, b: Option<bool>) -> Row {
        Row::new(vec![
            i.map_or(Value::Null, Value::Int),
            s.map_or(Value::Null, |x| Value::Str(x.to_owned())),
            b.map_or(Value::Null, Value::Bool),
        ])
    }

    fn run(expr: &Expr, r: &Row) -> Result<Value> {
        eval(expr, r, &scope())
    }

    fn col(name: &str) -> Expr {
        Expr::column(name)
    }

    #[test]
    fn s13_comparison_with_null_is_null() {
        let r = row(None, None, None);
        for op in [
            BinOp::Eq,
            BinOp::Ne,
            BinOp::Lt,
            BinOp::Le,
            BinOp::Gt,
            BinOp::Ge,
        ] {
            let e = Expr::binary(op, col("t.i"), Expr::int(1));
            assert_eq!(run(&e, &r).unwrap(), Value::Null, "{} with NULL", op.name());
        }
        // NULL = NULL is NULL, not true. This is the one people get wrong.
        let e = Expr::binary(BinOp::Eq, col("t.i"), Expr::Null(DataType::Int64));
        assert_eq!(run(&e, &r).unwrap(), Value::Null);
    }

    #[test]
    fn s14_arithmetic_with_null_is_null_and_does_not_raise() {
        let r = row(None, None, None);
        // NULL / 0 would be a division by zero if the operation were performed. It is not.
        let e = Expr::binary(BinOp::Div, col("t.i"), Expr::int(0));
        assert_eq!(run(&e, &r).unwrap(), Value::Null);
    }

    #[test]
    fn s15_kleene_truth_tables() {
        let t = Some(true);
        let f = Some(false);
        let n: Option<bool> = None;

        assert_eq!(kleene_and(t, t), Value::Bool(true));
        assert_eq!(kleene_and(t, f), Value::Bool(false));
        assert_eq!(kleene_and(t, n), Value::Null);
        assert_eq!(kleene_and(f, f), Value::Bool(false));
        // The catch: FALSE AND NULL is FALSE, not NULL.
        assert_eq!(kleene_and(f, n), Value::Bool(false));
        assert_eq!(kleene_and(n, n), Value::Null);

        assert_eq!(kleene_or(t, t), Value::Bool(true));
        assert_eq!(kleene_or(t, f), Value::Bool(true));
        // The catch: TRUE OR NULL is TRUE, not NULL.
        assert_eq!(kleene_or(t, n), Value::Bool(true));
        assert_eq!(kleene_or(f, f), Value::Bool(false));
        assert_eq!(kleene_or(f, n), Value::Null);
        assert_eq!(kleene_or(n, n), Value::Null);
    }

    #[test]
    fn s15_not_null_is_null() {
        let r = row(None, None, None);
        assert_eq!(run(&!col("t.b"), &r).unwrap(), Value::Null);
        let r = row(None, None, Some(true));
        assert_eq!(run(&!col("t.b"), &r).unwrap(), Value::Bool(false));
    }

    #[test]
    fn s15_and_does_not_short_circuit() {
        // FALSE AND (1/0) still raises: both operands are evaluated (S-15).
        let r = row(Some(0), None, None);
        let e = Expr::and(
            Expr::boolean(false),
            Expr::binary(
                BinOp::Eq,
                Expr::binary(BinOp::Div, Expr::int(1), col("t.i")),
                Expr::int(1),
            ),
        );
        assert_eq!(
            run(&e, &r).unwrap_err(),
            PlanError::DivisionByZero { op: "/" }
        );
    }

    #[test]
    fn s16_is_null_is_two_valued() {
        let r = row(None, None, None);
        assert_eq!(
            run(&Expr::is_null(col("t.i")), &r).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            run(&Expr::is_not_null(col("t.i")), &r).unwrap(),
            Value::Bool(false)
        );
        let r = row(Some(1), None, None);
        assert_eq!(
            run(&Expr::is_null(col("t.i")), &r).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn s17_where_keeps_true_only() {
        let scope = scope();
        // x = x is NULL when x is NULL, so the row does not survive.
        let predicate = Expr::binary(BinOp::Eq, col("t.i"), col("t.i"));
        assert!(!is_true(&predicate, &row(None, None, None), &scope).unwrap());
        assert!(is_true(&predicate, &row(Some(3), None, None), &scope).unwrap());
    }

    #[test]
    fn s18_case_takes_the_first_true_branch_and_skips_null_conditions() {
        let r = row(None, None, None);
        let e = Expr::Case {
            whens: vec![
                // A NULL condition is skipped, exactly like a false one.
                (
                    Expr::binary(BinOp::Eq, col("t.i"), Expr::int(1)),
                    Expr::int(10),
                ),
                (Expr::boolean(true), Expr::int(20)),
            ],
            otherwise: Some(Box::new(Expr::int(30))),
        };
        assert_eq!(run(&e, &r).unwrap(), Value::Int(20));
    }

    #[test]
    fn s18_case_without_else_is_null_when_nothing_matches() {
        let r = row(Some(5), None, None);
        let e = Expr::Case {
            whens: vec![(Expr::boolean(false), Expr::int(1))],
            otherwise: None,
        };
        assert_eq!(run(&e, &r).unwrap(), Value::Null);
    }

    #[test]
    fn s18_case_does_not_evaluate_the_branch_it_did_not_take() {
        // The unselected branch divides by zero; selecting the first branch must not raise.
        let r = row(Some(0), None, None);
        let e = Expr::Case {
            whens: vec![
                (Expr::boolean(true), Expr::int(1)),
                (
                    Expr::boolean(true),
                    Expr::binary(BinOp::Div, Expr::int(1), col("t.i")),
                ),
            ],
            otherwise: None,
        };
        assert_eq!(run(&e, &r).unwrap(), Value::Int(1));
    }

    #[test]
    fn s20_overflow_is_an_error_not_a_wrap() {
        let r = row(Some(i64::MAX), None, None);
        let e = Expr::binary(BinOp::Add, col("t.i"), Expr::int(1));
        assert_eq!(
            run(&e, &r).unwrap_err(),
            PlanError::ArithmeticOverflow { op: "+" }
        );
    }

    #[test]
    fn s21_division_and_modulo_by_zero_and_the_min_over_minus_one_case() {
        let r = row(Some(1), None, None);
        assert_eq!(
            run(&Expr::binary(BinOp::Div, col("t.i"), Expr::int(0)), &r).unwrap_err(),
            PlanError::DivisionByZero { op: "/" }
        );
        assert_eq!(
            run(&Expr::binary(BinOp::Mod, col("t.i"), Expr::int(0)), &r).unwrap_err(),
            PlanError::DivisionByZero { op: "%" }
        );

        let r = row(Some(i64::MIN), None, None);
        assert_eq!(
            run(&Expr::binary(BinOp::Div, col("t.i"), Expr::int(-1)), &r).unwrap_err(),
            PlanError::ArithmeticOverflow { op: "/" }
        );
        assert_eq!(
            run(&Expr::binary(BinOp::Mod, col("t.i"), Expr::int(-1)), &r).unwrap_err(),
            PlanError::ArithmeticOverflow { op: "%" }
        );
    }

    #[test]
    fn s21_division_truncates_toward_zero_and_modulo_takes_the_dividend_sign() {
        let r = row(Some(-7), None, None);
        assert_eq!(
            run(&Expr::binary(BinOp::Div, col("t.i"), Expr::int(2)), &r).unwrap(),
            Value::Int(-3)
        );
        assert_eq!(
            run(&Expr::binary(BinOp::Mod, col("t.i"), Expr::int(2)), &r).unwrap(),
            Value::Int(-1)
        );
    }

    #[test]
    fn strings_compare_byte_wise() {
        let r = row(None, Some("Z"), None);
        let e = Expr::binary(BinOp::Lt, col("t.s"), Expr::string("a"));
        assert_eq!(run(&e, &r).unwrap(), Value::Bool(true));
    }
}
