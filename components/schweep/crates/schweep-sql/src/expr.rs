//! SQL expression → [`schweep_plan::Expr`] (`docs/SEMANTICS.md` S-13 … S-19).
//!
//! The translation is deliberately narrow. Every accepted form maps to exactly one `Expr` variant,
//! and the shape of the accepted set is decided by what `docs/SEMANTICS.md` defines rather than by
//! what SQL happens to write: `IS TRUE` is not here, because there is no rule for it; `LIKE` is not
//! here, because there is no rule for it; `-x` is here only over an integer literal, because S-19's
//! table has no unary minus and a negative literal is a literal.
//!
//! ## The two rules that shape this file
//!
//! **A bare `NULL` is refused; a null is written `CAST(NULL AS <type>)` (S-19).** That is the only
//! accepted `CAST`. The alternative was to infer a null's type from its context, and it was rejected
//! because inference would be a *second* analysis of the query living only in the SQL door, while
//! the oracle types expressions by S-19's table. Two analyses that must agree are a disagreement
//! waiting to happen.
//!
//! **An aggregate is not a scalar expression.** `schweep_plan::Expr` has no aggregate variant at
//! all, so an aggregate met while translating a scalar is always a refusal — and *which* refusal
//! depends on where it was met (S-32). That is what [`Context`] carries: not a mode that changes the
//! translation, only the name the error will use, so that `WHERE COUNT(*) > 1` says the useful thing
//! about `WHERE` rather than a generic "aggregate not allowed".

use schweep_plan::plan::{AggFunc, BinOp, Expr};
use schweep_plan::PlanError;
use schweep_zset::{DataType, Value};
use sqlparser::ast::{
    self, BinaryOperator, CastKind, DuplicateTreatment, FunctionArg, FunctionArgExpr,
    FunctionArguments, UnaryOperator,
};

use crate::error::{Result, SqlError};

/// Where a scalar expression was found — used only to name the refusal if it holds an aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Where,
    Having,
    /// A select item, a GROUP BY key, or anything else where an aggregate would have to be nested
    /// inside an expression to appear at all.
    Scalar,
    /// Inside an aggregate's own argument: `SUM(COUNT(a))`.
    AggregateArgument {
        outer: &'static str,
    },
}

impl Context {
    /// The refusal for an aggregate found here (S-32).
    fn aggregate_refusal(self, func: String) -> SqlError {
        SqlError::Plan(match self {
            Context::Where => PlanError::AggregateInWhere { func },
            Context::Having => PlanError::AggregateInHaving { func },
            Context::Scalar => PlanError::AggregateNotTopLevel { func },
            Context::AggregateArgument { outer } => PlanError::NestedAggregate {
                outer: outer.to_owned(),
                inner: func,
            },
        })
    }
}

/// Translate a scalar expression. Aggregates are refused, by the name [`Context`] chooses.
pub fn scalar(expr: &ast::Expr, context: Context) -> Result<Expr> {
    match expr {
        // `t.a` — the only column form before a GROUP BY (S-10).
        ast::Expr::CompoundIdentifier(parts) => {
            let name = parts
                .iter()
                .map(|part| part.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            Ok(Expr::Column(name))
        }
        // `a` — legal only after a GROUP BY, where the binder's scope will say so (S-10, S-27). The
        // binder decides, not this function: refusing unqualified names here would report `HAVING n
        // > 1` as a syntax problem when it is the one place the form is correct.
        ast::Expr::Identifier(ident) => Ok(Expr::Column(ident.value.clone())),

        ast::Expr::Nested(inner) => scalar(inner, context),

        ast::Expr::Value(value) => literal(&value.value),

        // A negative literal is a literal, not an operator applied to one (S-19 has no unary minus).
        ast::Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => match negated_int(inner) {
            Some(literal) => Ok(literal),
            None => Err(SqlError::NotInDialect("unary minus")),
        },
        ast::Expr::UnaryOp {
            op: UnaryOperator::Plus,
            ..
        } => Err(SqlError::NotInDialect("unary plus")),
        ast::Expr::UnaryOp {
            op: UnaryOperator::Not | UnaryOperator::BangNot,
            expr: inner,
        } => Ok(!scalar(inner, context)?),
        ast::Expr::UnaryOp { .. } => Err(SqlError::NotInDialect("that unary operator")),

        ast::Expr::BinaryOp { left, op, right } => {
            let l = scalar(left, context)?;
            let r = scalar(right, context)?;
            match op {
                // Kleene `AND`/`OR`, which do not short-circuit (S-15).
                BinaryOperator::And => Ok(Expr::and(l, r)),
                BinaryOperator::Or => Ok(Expr::or(l, r)),
                _ => Ok(Expr::binary(binary_op(op)?, l, r)),
            }
        }

        ast::Expr::IsNull(inner) => Ok(Expr::is_null(scalar(inner, context)?)),
        ast::Expr::IsNotNull(inner) => Ok(Expr::is_not_null(scalar(inner, context)?)),

        ast::Expr::Case {
            case_token: _,
            end_token: _,
            operand,
            conditions,
            else_result,
        } => {
            // `CASE x WHEN 1 THEN ...` is sugar for `CASE WHEN x = 1 THEN ...`, and desugaring it
            // would duplicate `x` into every branch — which S-18 says is evaluated, so a `CASE`
            // whose operand raises would raise once per branch. The searched form is the one with
            // a rule; the simple form is refused rather than rewritten.
            if operand.is_some() {
                return Err(SqlError::NotInDialect("CASE with an operand"));
            }
            let mut whens = Vec::with_capacity(conditions.len());
            for when in conditions {
                whens.push((
                    scalar(&when.condition, context)?,
                    scalar(&when.result, context)?,
                ));
            }
            let otherwise = match else_result {
                None => None,
                Some(e) => Some(Box::new(scalar(e, context)?)),
            };
            Ok(Expr::Case { whens, otherwise })
        }

        // The one accepted cast: it types a null literal rather than converting anything (S-19).
        ast::Expr::Cast {
            kind,
            expr: inner,
            data_type,
            array,
            format,
        } => {
            if *kind != CastKind::Cast || *array || format.is_some() {
                return Err(SqlError::UnsupportedCast(expr.to_string()));
            }
            match inner.as_ref() {
                ast::Expr::Value(v) if matches!(v.value, ast::Value::Null) => {
                    Ok(Expr::Null(type_name(data_type)?))
                }
                _ => Err(SqlError::UnsupportedCast(expr.to_string())),
            }
        }

        ast::Expr::Function(function) => {
            let name = function_name(function);
            Err(context.aggregate_refusal(name))
        }

        ast::Expr::Subquery(_) | ast::Expr::Exists { .. } | ast::Expr::InSubquery { .. } => {
            Err(SqlError::NotInDialect("a subquery"))
        }
        ast::Expr::InList { .. } => Err(SqlError::NotInDialect("IN")),
        ast::Expr::Between { .. } => Err(SqlError::NotInDialect("BETWEEN")),
        ast::Expr::Like { .. } | ast::Expr::ILike { .. } => Err(SqlError::NotInDialect("LIKE")),
        ast::Expr::IsTrue(_)
        | ast::Expr::IsNotTrue(_)
        | ast::Expr::IsFalse(_)
        | ast::Expr::IsNotFalse(_)
        | ast::Expr::IsUnknown(_)
        | ast::Expr::IsNotUnknown(_) => Err(SqlError::NotInDialect("IS TRUE / FALSE / UNKNOWN")),
        ast::Expr::IsDistinctFrom(_, _) | ast::Expr::IsNotDistinctFrom(_, _) => {
            Err(SqlError::NotInDialect("IS [NOT] DISTINCT FROM"))
        }
        ast::Expr::Collate { .. } => Err(SqlError::NotInDialect("COLLATE")),
        // sqlparser parses `GROUP BY ROLLUP (a)` as an *expression* in the grouping list rather than
        // as a modifier, so the refusal has to live here as well as in `parse`.
        ast::Expr::Rollup(_) | ast::Expr::Cube(_) | ast::Expr::GroupingSets(_) => {
            Err(SqlError::NotInDialect("ROLLUP / CUBE / GROUPING SETS"))
        }
        ast::Expr::Wildcard(_) | ast::Expr::QualifiedWildcard(_, _) => {
            Err(SqlError::SelectStarNotSupported)
        }
        _ => Err(SqlError::NotInDialect("that expression")),
    }
}

/// The aggregate a select item is, if it is exactly one (S-30).
///
/// Returns `None` for anything that is not a function call, so the caller can treat the item as a
/// scalar — and a scalar that *contains* an aggregate is refused by [`scalar`], which is where the
/// `AggregateNotTopLevel` refusal comes from.
pub fn as_aggregate(expr: &ast::Expr) -> Option<&ast::Function> {
    match expr {
        ast::Expr::Function(function) => Some(function),
        ast::Expr::Nested(inner) => as_aggregate(inner),
        _ => None,
    }
}

/// Translate a function call to one of the six aggregates (S-30), refusing everything else.
pub fn aggregate(function: &ast::Function) -> Result<AggFunc> {
    let ast::Function {
        name,
        uses_odbc_syntax,
        parameters,
        args,
        filter,
        null_treatment,
        over,
        within_group,
    } = function;

    let rendered = function_name(function);
    if *uses_odbc_syntax {
        return Err(SqlError::NotInDialect("ODBC function syntax"));
    }
    if !matches!(parameters, FunctionArguments::None) {
        return Err(SqlError::NotInDialect("parametric aggregates"));
    }
    if filter.is_some() {
        return Err(SqlError::NotInDialect("FILTER on an aggregate"));
    }
    if null_treatment.is_some() {
        return Err(SqlError::NotInDialect("IGNORE / RESPECT NULLS"));
    }
    if over.is_some() {
        return Err(SqlError::NotInDialect("a window function (OVER)"));
    }
    if !within_group.is_empty() {
        return Err(SqlError::NotInDialect("WITHIN GROUP"));
    }

    // Function names are language, not data: SQL keywords are case-insensitive, so `count(*)` and
    // `COUNT(*)` are the same function. That is not in tension with S-11's verbatim identifiers —
    // an identifier names something the user created, and a function name names something the
    // dialect defines.
    // The *value*, not the rendering: `"COUNT"(x)` would render with its quotes. A function name is
    // language rather than data, and SQL keywords are case-insensitive, so it folds (S-11).
    let upper = match name.0.as_slice() {
        [ast::ObjectNamePart::Identifier(ident)] => ident.value.to_ascii_uppercase(),
        _ => return Err(SqlError::UnknownFunction(rendered)),
    };

    let list = match args {
        FunctionArguments::List(list) => list,
        FunctionArguments::None => return Err(SqlError::UnknownFunction(rendered)),
        FunctionArguments::Subquery(_) => return Err(SqlError::NotInDialect("a subquery")),
    };
    if !list.clauses.is_empty() {
        return Err(SqlError::NotInDialect("ORDER BY inside an aggregate"));
    }
    if matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)) {
        // `COUNT(DISTINCT x)` is a different aggregate — it needs the distinct set per group, which
        // is state S-30's rules do not describe. Rung 4's `DISTINCT` does not give it for free.
        return Err(SqlError::NotInDialect("DISTINCT inside an aggregate"));
    }

    let star = list
        .args
        .iter()
        .any(|arg| matches!(arg, FunctionArg::Unnamed(FunctionArgExpr::Wildcard)));
    if upper == "COUNT" && star && list.args.len() == 1 {
        return Ok(AggFunc::CountStar);
    }

    let argument = match (list.args.len(), list.args.first()) {
        (1, Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e)))) => e,
        _ => return Err(SqlError::UnknownFunction(rendered)),
    };
    let outer: &'static str = match upper.as_str() {
        "COUNT" => "COUNT",
        "SUM" => "SUM",
        "MIN" => "MIN",
        "MAX" => "MAX",
        "AVG" => "AVG",
        _ => return Err(SqlError::UnknownFunction(rendered)),
    };
    let inner = scalar(argument, Context::AggregateArgument { outer })?;
    Ok(match outer {
        "COUNT" => AggFunc::Count(inner),
        "SUM" => AggFunc::Sum(inner),
        "MIN" => AggFunc::Min(inner),
        "MAX" => AggFunc::Max(inner),
        // `outer` was matched to exactly these five above.
        _ => AggFunc::Avg(inner),
    })
}

/// How an aggregate call is named in a refusal: `COUNT(*)`, not `Function { .. }`.
fn function_name(function: &ast::Function) -> String {
    function.to_string()
}

fn binary_op(op: &BinaryOperator) -> Result<BinOp> {
    Ok(match op {
        BinaryOperator::Plus => BinOp::Add,
        BinaryOperator::Minus => BinOp::Sub,
        BinaryOperator::Multiply => BinOp::Mul,
        BinaryOperator::Divide => BinOp::Div,
        BinaryOperator::Modulo => BinOp::Mod,
        BinaryOperator::Eq => BinOp::Eq,
        BinaryOperator::NotEq => BinOp::Ne,
        BinaryOperator::Lt => BinOp::Lt,
        BinaryOperator::LtEq => BinOp::Le,
        BinaryOperator::Gt => BinOp::Gt,
        BinaryOperator::GtEq => BinOp::Ge,
        BinaryOperator::StringConcat => return Err(SqlError::NotInDialect("||")),
        BinaryOperator::Spaceship => return Err(SqlError::NotInDialect("<=>")),
        BinaryOperator::Xor => return Err(SqlError::NotInDialect("XOR")),
        _ => return Err(SqlError::NotInDialect("that binary operator")),
    })
}

/// A literal, with the two SQL forms this dialect does not have refused (S-1, S-3, S-19).
fn literal(value: &ast::Value) -> Result<Expr> {
    match value {
        ast::Value::Number(text, _) => int_literal(text).map(Expr::Literal),
        ast::Value::SingleQuotedString(s) => Ok(Expr::string(s.clone())),
        ast::Value::Boolean(b) => Ok(Expr::boolean(*b)),
        // The refusal S-19 requires: a null with no type, and nothing here to infer one from.
        ast::Value::Null => Err(SqlError::Plan(PlanError::UntypedNullLiteral)),
        ast::Value::Placeholder(_) => Err(SqlError::NotInDialect("a bind placeholder")),
        _ => Err(SqlError::NotInDialect("that literal")),
    }
}

fn int_literal(text: &str) -> Result<Value> {
    if text.contains(['.', 'e', 'E']) {
        // S-3: `Float64` is a *result* type, produced only by AVG. There are no float literals.
        return Err(SqlError::Plan(PlanError::NotInDialect("a FLOAT literal")));
    }
    text.parse::<i64>()
        .map(Value::Int)
        .map_err(|_| SqlError::NumberOutOfRange(text.to_owned()))
}

/// `-<integer literal>`, folded into the literal itself.
///
/// `i64::MIN` is the reason this exists as a fold rather than a negation applied afterwards:
/// `9223372036854775808` does not parse as an `i64`, so `-9223372036854775808` could only be
/// written if the sign is part of the literal.
fn negated_int(inner: &ast::Expr) -> Option<Expr> {
    match inner {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(text, _) if !text.contains(['.', 'e', 'E']) => {
                let negated = format!("-{text}");
                negated.parse::<i64>().ok().map(Expr::int)
            }
            _ => None,
        },
        ast::Expr::Nested(deeper) => negated_int(deeper),
        _ => None,
    }
}

/// The four type names a `CAST(NULL AS ...)` may use (S-2, S-3).
fn type_name(data_type: &ast::DataType) -> Result<DataType> {
    match data_type {
        ast::DataType::BigInt(_) | ast::DataType::Int64 => Ok(DataType::Int64),
        ast::DataType::Text | ast::DataType::Varchar(_) => Ok(DataType::Utf8),
        ast::DataType::Boolean | ast::DataType::Bool => Ok(DataType::Boolean),
        // Float64 exists, but only as AVG's result (S-3), so there is no way to write one.
        other => Err(SqlError::UnsupportedTypeName(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(text: &str) -> ast::Expr {
        let dialect = GenericDialect {};
        Parser::new(&dialect)
            .try_with_sql(text)
            .unwrap()
            .parse_expr()
            .unwrap()
    }

    fn translate(text: &str) -> Result<Expr> {
        scalar(&parse_expr(text), Context::Scalar)
    }

    #[test]
    fn columns_keep_their_qualification_verbatim() {
        assert_eq!(translate("t.a").unwrap(), Expr::column("t.a"));
        assert_eq!(translate("T.A").unwrap(), Expr::column("T.A"));
        assert_eq!(translate("n").unwrap(), Expr::column("n"));
        assert_eq!(translate("\"odd name\"").unwrap(), Expr::column("odd name"));
    }

    #[test]
    fn the_operators_in_s19s_table_translate_and_nothing_else_does() {
        assert_eq!(
            translate("t.a + 1").unwrap(),
            Expr::binary(BinOp::Add, Expr::column("t.a"), Expr::int(1))
        );
        assert_eq!(
            translate("t.a <> 'x'").unwrap(),
            Expr::binary(BinOp::Ne, Expr::column("t.a"), Expr::string("x"))
        );
        assert_eq!(
            translate("t.a IS NULL AND NOT t.b").unwrap(),
            Expr::and(Expr::is_null(Expr::column("t.a")), !Expr::column("t.b"))
        );
        assert_eq!(
            translate("t.a || t.b").unwrap_err(),
            SqlError::NotInDialect("||")
        );
        assert_eq!(
            translate("t.a LIKE 'x%'").unwrap_err(),
            SqlError::NotInDialect("LIKE")
        );
        assert_eq!(
            translate("t.a BETWEEN 1 AND 2").unwrap_err(),
            SqlError::NotInDialect("BETWEEN")
        );
        assert_eq!(
            translate("t.a IN (1, 2)").unwrap_err(),
            SqlError::NotInDialect("IN")
        );
        assert_eq!(
            translate("t.a IS TRUE").unwrap_err(),
            SqlError::NotInDialect("IS TRUE / FALSE / UNKNOWN")
        );
    }

    /// A negative literal is a literal — including the one that cannot be written any other way.
    #[test]
    fn negative_integer_literals_fold_into_the_literal() {
        assert_eq!(translate("-1").unwrap(), Expr::int(-1));
        assert_eq!(
            translate("-9223372036854775808").unwrap(),
            Expr::int(i64::MIN)
        );
        assert_eq!(
            translate("9223372036854775808").unwrap_err(),
            SqlError::NumberOutOfRange("9223372036854775808".to_owned())
        );
        assert_eq!(
            translate("-t.a").unwrap_err(),
            SqlError::NotInDialect("unary minus")
        );
    }

    #[test]
    fn a_bare_null_is_refused_and_a_cast_null_is_typed() {
        assert_eq!(
            translate("NULL").unwrap_err(),
            SqlError::Plan(PlanError::UntypedNullLiteral)
        );
        assert_eq!(
            translate("CAST(NULL AS BIGINT)").unwrap(),
            Expr::Null(DataType::Int64)
        );
        assert_eq!(
            translate("CAST(NULL AS TEXT)").unwrap(),
            Expr::Null(DataType::Utf8)
        );
        assert_eq!(
            translate("CAST(NULL AS BOOLEAN)").unwrap(),
            Expr::Null(DataType::Boolean)
        );
        assert_eq!(
            translate("CAST(NULL AS DOUBLE)").unwrap_err(),
            SqlError::UnsupportedTypeName("DOUBLE".to_owned())
        );
    }

    #[test]
    fn a_cast_that_converts_is_refused() {
        assert!(matches!(
            translate("CAST(t.a AS TEXT)").unwrap_err(),
            SqlError::UnsupportedCast(_)
        ));
        assert!(matches!(
            translate("TRY_CAST(NULL AS BIGINT)").unwrap_err(),
            SqlError::UnsupportedCast(_)
        ));
    }

    #[test]
    fn float_literals_are_refused_because_float_is_a_result_type() {
        assert_eq!(
            translate("1.5").unwrap_err(),
            SqlError::Plan(PlanError::NotInDialect("a FLOAT literal"))
        );
    }

    /// The same aggregate, met in three places, is refused by three different names (S-32).
    #[test]
    fn an_aggregate_is_refused_by_the_name_of_where_it_was_found() {
        let count = parse_expr("COUNT(*) > 1");
        assert_eq!(
            scalar(&count, Context::Where).unwrap_err(),
            SqlError::Plan(PlanError::AggregateInWhere {
                func: "COUNT(*)".to_owned()
            })
        );
        assert_eq!(
            scalar(&count, Context::Having).unwrap_err(),
            SqlError::Plan(PlanError::AggregateInHaving {
                func: "COUNT(*)".to_owned()
            })
        );
        assert_eq!(
            scalar(&count, Context::Scalar).unwrap_err(),
            SqlError::Plan(PlanError::AggregateNotTopLevel {
                func: "COUNT(*)".to_owned()
            })
        );
    }

    #[test]
    fn the_six_aggregates_translate() {
        for (text, expected) in [
            ("COUNT(*)", AggFunc::CountStar),
            ("count(*)", AggFunc::CountStar),
            ("COUNT(t.a)", AggFunc::Count(Expr::column("t.a"))),
            ("SUM(t.a)", AggFunc::Sum(Expr::column("t.a"))),
            ("MIN(t.a)", AggFunc::Min(Expr::column("t.a"))),
            ("MAX(t.a)", AggFunc::Max(Expr::column("t.a"))),
            ("AVG(t.a)", AggFunc::Avg(Expr::column("t.a"))),
        ] {
            let parsed = parse_expr(text);
            let function = as_aggregate(&parsed).unwrap();
            assert_eq!(aggregate(function).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn an_aggregate_inside_an_aggregate_names_both() {
        let parsed = parse_expr("SUM(COUNT(t.a))");
        let function = as_aggregate(&parsed).unwrap();
        assert_eq!(
            aggregate(function).unwrap_err(),
            SqlError::Plan(PlanError::NestedAggregate {
                outer: "SUM".to_owned(),
                inner: "COUNT(t.a)".to_owned()
            })
        );
    }

    #[test]
    fn aggregate_decorations_are_refused_by_name() {
        for (text, construct) in [
            ("COUNT(DISTINCT t.a)", "DISTINCT inside an aggregate"),
            (
                "COUNT(t.a) FILTER (WHERE t.a > 1)",
                "FILTER on an aggregate",
            ),
            ("SUM(t.a) OVER ()", "a window function (OVER)"),
        ] {
            let parsed = parse_expr(text);
            let function = as_aggregate(&parsed).unwrap();
            assert_eq!(
                aggregate(function).unwrap_err(),
                SqlError::NotInDialect(construct),
                "{text}"
            );
        }
    }

    #[test]
    fn an_unknown_function_names_itself() {
        let parsed = parse_expr("LOWER(t.a)");
        let function = as_aggregate(&parsed).unwrap();
        assert_eq!(
            aggregate(function).unwrap_err(),
            SqlError::UnknownFunction("LOWER(t.a)".to_owned())
        );
    }

    #[test]
    fn case_translates_only_in_its_searched_form() {
        assert_eq!(
            translate("CASE WHEN t.a > 1 THEN 2 ELSE 3 END").unwrap(),
            Expr::Case {
                whens: vec![(
                    Expr::binary(BinOp::Gt, Expr::column("t.a"), Expr::int(1)),
                    Expr::int(2)
                )],
                otherwise: Some(Box::new(Expr::int(3))),
            }
        );
        assert_eq!(
            translate("CASE t.a WHEN 1 THEN 2 END").unwrap_err(),
            SqlError::NotInDialect("CASE with an operand")
        );
    }
}
