//! The five aggregates plus `COUNT(*)` (`docs/SEMANTICS.md` S-30, S-31).
//!
//! Two things distinguish these from a textbook implementation, and both are Z-set facts:
//!
//! 1. **Weights are multiplicities, so they count.** A row at weight 3 is three rows.
//!    `COUNT(*)` of it is 3 and `SUM(x)` of it is `3·x`. `MIN`/`MAX` are unaffected by how many
//!    copies there are — but a value must be *present* (weight ≥ 1) to be considered.
//! 2. **Nothing here looks at the sign of a weight** (I-5). Group members arrive consolidated
//!    and non-negative; a negative weight would be an oracle bug, and the caller reports it as
//!    one rather than quietly folding it in.

use schweep_plan::eval::eval;
use schweep_plan::plan::AggFunc;
use schweep_plan::PlanError;
use schweep_zset::{Row, Schema, Value};

use crate::error::{OracleError, Result};

/// Evaluate one aggregate over the members of a group.
///
/// `members` are `(row, weight)` entries with weight ≥ 1, all belonging to the same group.
pub fn evaluate(func: &AggFunc, members: &[(Row, i64)], scope: &Schema) -> Result<Value> {
    let name = func.name();
    match func {
        // COUNT(*) counts every entry by weight, null or not: it counts rows, not values.
        AggFunc::CountStar => {
            let mut total: i64 = 0;
            for (_, weight) in members {
                total = total.checked_add(*weight).ok_or(OracleError::Plan(
                    PlanError::AggregateOverflow { func: name },
                ))?;
            }
            Ok(Value::Int(total))
        }

        // COUNT(x) counts entries whose value is not null — and returns 0, never null, when
        // there are none (S-30). That asymmetry with SUM is SQL's, and it is intentional.
        AggFunc::Count(arg) => {
            let mut total: i64 = 0;
            for (row, weight) in members {
                if !eval(arg, row, scope)?.is_null() {
                    total = total.checked_add(*weight).ok_or(OracleError::Plan(
                        PlanError::AggregateOverflow { func: name },
                    ))?;
                }
            }
            Ok(Value::Int(total))
        }

        AggFunc::Sum(arg) => match sum_and_count(arg, members, scope, name)? {
            None => Ok(Value::Null),
            Some((sum, _)) => Ok(Value::Int(sum)),
        },

        // AVG is one IEEE-754 division of two exact integers, performed once at emit time and
        // never accumulated (S-31, D-10). This is precisely why AVG can be a Float64 while a
        // float SUM cannot exist: identical integer inputs through one identical division give
        // identical bits in every implementation, which is what I-1 demands.
        AggFunc::Avg(arg) => match sum_and_count(arg, members, scope, name)? {
            // Empty P: no division is performed, so AVG never divides by zero (S-31).
            None => Ok(Value::Null),
            Some((sum, count)) => Ok(Value::Float(sum as f64 / count as f64)),
        },

        AggFunc::Min(arg) => Ok(extremum(arg, members, scope, Extremum::Min)?),
        AggFunc::Max(arg) => Ok(extremum(arg, members, scope, Extremum::Max)?),
    }
}

/// The exact weighted sum and the exact count of the non-null values in a group.
///
/// Returns `None` when there are no non-null values, which is the shared "empty P" case for
/// `SUM` and `AVG` (S-30).
///
/// The accumulator is `i128`. A `SUM` under retraction can transit large partial values and
/// still land inside `Int64` — the wider accumulator means such a sum is correct rather than a
/// spurious overflow, and the final narrowing to `Int64` is checked (S-30, D-11).
fn sum_and_count(
    arg: &schweep_plan::plan::Expr,
    members: &[(Row, i64)],
    scope: &Schema,
    func: &'static str,
) -> Result<Option<(i64, i64)>> {
    let mut sum: i128 = 0;
    let mut count: i64 = 0;
    let mut any = false;

    for (row, weight) in members {
        let value = eval(arg, row, scope)?;
        let x = match value {
            Value::Null => continue,
            Value::Int(x) => x,
            // Unreachable after binding, which proves SUM/AVG arguments are Int64 (S-30).
            other => {
                return Err(OracleError::Plan(PlanError::AggregateTypeUnsupported {
                    func,
                    ty: other.data_type().unwrap_or(schweep_zset::DataType::Int64),
                }))
            }
        };
        any = true;
        let term = i128::from(*weight)
            .checked_mul(i128::from(x))
            .ok_or(OracleError::Plan(PlanError::AggregateOverflow { func }))?;
        sum = sum
            .checked_add(term)
            .ok_or(OracleError::Plan(PlanError::AggregateOverflow { func }))?;
        count = count
            .checked_add(*weight)
            .ok_or(OracleError::Plan(PlanError::AggregateOverflow { func }))?;
    }

    if !any {
        return Ok(None);
    }
    let sum =
        i64::try_from(sum).map_err(|_| OracleError::Plan(PlanError::AggregateOverflow { func }))?;
    Ok(Some((sum, count)))
}

#[derive(Clone, Copy)]
enum Extremum {
    Min,
    Max,
}

/// `MIN`/`MAX` over the non-null values of a group, by the total order on values (S-7).
///
/// Multiplicity is irrelevant here — a value present once and a value present three times are
/// equally present — but presence is not: an entry contributes only if its weight is at least 1.
/// Group members are consolidated and non-negative, so that condition is "the row is here at
/// all", which is why this function does not inspect the sign of a weight either.
fn extremum(
    arg: &schweep_plan::plan::Expr,
    members: &[(Row, i64)],
    scope: &Schema,
    which: Extremum,
) -> Result<Value> {
    let mut best: Option<Value> = None;
    for (row, weight) in members {
        if *weight < 1 {
            continue;
        }
        let value = eval(arg, row, scope)?;
        if value.is_null() {
            continue;
        }
        best = Some(match best {
            None => value,
            Some(current) => match which {
                Extremum::Min => {
                    if value < current {
                        value
                    } else {
                        current
                    }
                }
                Extremum::Max => {
                    if value > current {
                        value
                    } else {
                        current
                    }
                }
            },
        });
    }
    Ok(best.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_plan::plan::Expr;
    use schweep_zset::{DataType, Field};

    fn scope() -> Schema {
        Schema::new(vec![
            Field::nullable("t.x", DataType::Int64),
            Field::nullable("t.s", DataType::Utf8),
        ])
        .unwrap()
    }

    fn member(x: Option<i64>, s: Option<&str>, weight: i64) -> (Row, i64) {
        (
            Row::new(vec![
                x.map_or(Value::Null, Value::Int),
                s.map_or(Value::Null, |v| Value::Str(v.to_owned())),
            ]),
            weight,
        )
    }

    fn agg(func: AggFunc, members: &[(Row, i64)]) -> Result<Value> {
        evaluate(&func, members, &scope())
    }

    fn x() -> Expr {
        Expr::column("t.x")
    }

    #[test]
    fn s30_weights_are_multiplicities() {
        let members = [member(Some(5), None, 3)];
        assert_eq!(agg(AggFunc::CountStar, &members).unwrap(), Value::Int(3));
        assert_eq!(agg(AggFunc::Count(x()), &members).unwrap(), Value::Int(3));
        assert_eq!(agg(AggFunc::Sum(x()), &members).unwrap(), Value::Int(15));
        // MIN/MAX are unaffected by multiplicity.
        assert_eq!(agg(AggFunc::Min(x()), &members).unwrap(), Value::Int(5));
        assert_eq!(agg(AggFunc::Max(x()), &members).unwrap(), Value::Int(5));
        assert_eq!(agg(AggFunc::Avg(x()), &members).unwrap(), Value::Float(5.0));
    }

    #[test]
    fn s30_nulls_are_ignored_but_count_star_still_counts_them() {
        let members = [
            member(None, None, 2),
            member(Some(4), None, 1),
            member(Some(6), None, 1),
        ];
        assert_eq!(agg(AggFunc::CountStar, &members).unwrap(), Value::Int(4));
        assert_eq!(agg(AggFunc::Count(x()), &members).unwrap(), Value::Int(2));
        assert_eq!(agg(AggFunc::Sum(x()), &members).unwrap(), Value::Int(10));
        assert_eq!(agg(AggFunc::Avg(x()), &members).unwrap(), Value::Float(5.0));
    }

    #[test]
    fn s30_an_all_null_group_counts_zero_but_sums_to_null() {
        let members = [member(None, None, 3)];
        assert_eq!(agg(AggFunc::Count(x()), &members).unwrap(), Value::Int(0));
        assert_eq!(agg(AggFunc::Sum(x()), &members).unwrap(), Value::Null);
        assert_eq!(agg(AggFunc::Min(x()), &members).unwrap(), Value::Null);
        assert_eq!(agg(AggFunc::Max(x()), &members).unwrap(), Value::Null);
        assert_eq!(agg(AggFunc::Avg(x()), &members).unwrap(), Value::Null);
        // COUNT(*) still sees the rows themselves.
        assert_eq!(agg(AggFunc::CountStar, &members).unwrap(), Value::Int(3));
    }

    #[test]
    fn s31_avg_never_divides_by_zero() {
        // The only route to a zero denominator is an empty P, and that returns NULL first.
        let members = [member(None, None, 7)];
        assert_eq!(agg(AggFunc::Avg(x()), &members).unwrap(), Value::Null);
    }

    #[test]
    fn s31_avg_is_the_exact_sum_over_the_exact_count() {
        let members = [member(Some(1), None, 1), member(Some(2), None, 1)];
        assert_eq!(agg(AggFunc::Avg(x()), &members).unwrap(), Value::Float(1.5));
        // Weighted: 1 appears 3 times, 2 appears once -> 5/4.
        let members = [member(Some(1), None, 3), member(Some(2), None, 1)];
        assert_eq!(
            agg(AggFunc::Avg(x()), &members).unwrap(),
            Value::Float(1.25)
        );
    }

    #[test]
    fn s30_min_max_use_the_total_order_and_work_on_strings() {
        let s = Expr::column("t.s");
        let members = [
            member(None, Some("b"), 1),
            member(None, Some("Z"), 1),
            member(None, None, 1),
        ];
        // Byte-wise: "Z" < "b".
        assert_eq!(
            agg(AggFunc::Min(s.clone()), &members).unwrap(),
            Value::Str("Z".into())
        );
        assert_eq!(
            agg(AggFunc::Max(s), &members).unwrap(),
            Value::Str("b".into())
        );
    }

    #[test]
    fn s30_sum_transits_a_wide_partial_value_and_still_lands_in_range() {
        // (i64::MAX) + (i64::MAX) - (i64::MAX) = i64::MAX. An i64 accumulator would overflow
        // halfway; the i128 accumulator does not, and the exact total is in range.
        let members = [
            member(Some(i64::MAX), None, 2),
            member(Some(-i64::MAX), None, 1),
        ];
        assert_eq!(
            agg(AggFunc::Sum(x()), &members).unwrap(),
            Value::Int(i64::MAX)
        );
    }

    #[test]
    fn s30_sum_out_of_range_is_an_error_not_a_wrap() {
        let members = [member(Some(i64::MAX), None, 2)];
        assert_eq!(
            agg(AggFunc::Sum(x()), &members).unwrap_err(),
            OracleError::Plan(PlanError::AggregateOverflow { func: "SUM" })
        );
    }

    #[test]
    fn an_empty_group_aggregates_to_the_empty_answers() {
        // Not reachable through the engine — a group with no members does not exist (S-29) —
        // but the function must still be total.
        let members: [(Row, i64); 0] = [];
        assert_eq!(agg(AggFunc::CountStar, &members).unwrap(), Value::Int(0));
        assert_eq!(agg(AggFunc::Sum(x()), &members).unwrap(), Value::Null);
    }
}
