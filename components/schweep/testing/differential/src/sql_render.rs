//! Typed [`Query`] → SQL text, so the fuzzer can drive the SQL door with the population the
//! generator already produces.
//!
//! ## Why this is a renderer and not a second generator
//!
//! C5's gate wants "randomized queries over randomized schemas … engine-vs-oracle through the SQL
//! door", *and* I-6: the two doors compile to structurally identical plans. A second, independent SQL
//! generator would give the first without the second — there would be no typed query to compare the
//! SQL plan against. Rendering the existing population gives both from one source of randomness, and
//! it puts the renderer under the I-6 assertion: if this file writes SQL that means something else,
//! the plans differ and the gate fails with both trees printed.
//!
//! ## Why it lives in the test harness
//!
//! Nothing in the engine needs to *write* SQL. A renderer in `schweep-sql` would be production code
//! whose only caller is a test, and its bugs would then be shipped rather than merely noticed.
//!
//! ## What it declines, and why declining is honest
//!
//! Not every typed query has a SQL form in this dialect, and the ones that do not are **counted and
//! reported** rather than quietly dropped ([`NoSqlForm`]). A gate that silently skipped half its
//! population would read as "the SQL door is covered" while covering whatever was easy.

use std::fmt::Write as _;

use schweep_plan::plan::{AggFunc, Expr, GroupBy, Named, Query, Source};
use schweep_zset::{DataType, Value};

/// Why a typed query has no SQL form. Each variant is a real limit, stated as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoSqlForm {
    /// The answer is the input schema under its qualified names (`a.c0`, …). Only `SELECT *` could
    /// ask for that, and `SELECT *` is refused (S-11).
    WholeInputSchema,
    /// A projection *over* a GROUP BY. In SQL a group key's output name comes from the select list
    /// (S-11), so a query that both groups and projects would have to name its keys twice — once for
    /// the group output and once for the projection — and SQL has one select list to do it in. The
    /// typed API can, which is a real difference in reach and not a defect in either door.
    ProjectionOverGroupBy,
    /// `FROM a JOIN (b JOIN c)`. SQL's join list is a flat left-associative chain, so this would
    /// re-associate to `(a JOIN b) JOIN c` — a different plan for the same text.
    RightNestedJoin,
    /// Two group keys with the same expression. The SQL binder names a key from the *first* select
    /// item that asks for it, so both keys would take one name (S-11).
    DuplicateGroupKeyExpression,
}

impl NoSqlForm {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            NoSqlForm::WholeInputSchema => "no projection and no GROUP BY (would need SELECT *)",
            NoSqlForm::ProjectionOverGroupBy => "a projection over a GROUP BY",
            NoSqlForm::RightNestedJoin => "a right-nested join",
            NoSqlForm::DuplicateGroupKeyExpression => "two group keys with one expression",
        }
    }
}

/// Render a typed query as SQL, or say why it has no SQL form.
///
/// The rendering is deliberately over-parenthesised and over-quoted: every operand is wrapped, and
/// every identifier is double-quoted. Precedence and keyword collisions are then not this file's
/// problem, and cannot silently change what the text means.
pub fn sql_form(query: &Query) -> Result<String, NoSqlForm> {
    match (&query.group_by, &query.project) {
        (Some(_), Some(_)) => return Err(NoSqlForm::ProjectionOverGroupBy),
        (None, None) => return Err(NoSqlForm::WholeInputSchema),
        _ => {}
    }

    let mut sql = String::from("SELECT ");
    if query.distinct {
        sql.push_str("DISTINCT ");
    }

    let mut items: Vec<String> = Vec::new();
    if let Some(group_by) = &query.group_by {
        check_distinct_keys(group_by)?;
        // Keys then aggregates, in exactly the order the GROUP BY operator emits them, so the SQL
        // binder emits no projection (S-36) and the two plans match node for node.
        for key in &group_by.keys {
            items.push(format!("{} AS {}", expr_sql(&key.value), ident(&key.name)));
        }
        for agg in &group_by.aggregates {
            items.push(format!("{} AS {}", agg_sql(&agg.value), ident(&agg.name)));
        }
    }
    if let Some(project) = &query.project {
        for item in project {
            items.push(format!(
                "{} AS {}",
                expr_sql(&item.value),
                ident(&item.name)
            ));
        }
    }
    sql.push_str(&items.join(", "));

    let _ = write!(sql, " FROM {}", source_sql(&query.source, true)?);

    if let Some(predicate) = &query.filter {
        let _ = write!(sql, " WHERE {}", expr_sql(predicate));
    }

    if let Some(group_by) = &query.group_by {
        if !group_by.keys.is_empty() {
            let keys: Vec<String> = group_by.keys.iter().map(|k| expr_sql(&k.value)).collect();
            let _ = write!(sql, " GROUP BY {}", keys.join(", "));
        }
        if let Some(having) = &group_by.having {
            let _ = write!(sql, " HAVING {}", expr_sql(having));
        }
    }

    Ok(sql)
}

fn check_distinct_keys(group_by: &GroupBy) -> Result<(), NoSqlForm> {
    for (index, key) in group_by.keys.iter().enumerate() {
        if group_by
            .keys
            .get(..index)
            .is_some_and(|prior| prior.iter().any(|other| other.value == key.value))
        {
            return Err(NoSqlForm::DuplicateGroupKeyExpression);
        }
    }
    Ok(())
}

fn source_sql(source: &Source, is_root: bool) -> Result<String, NoSqlForm> {
    match source {
        Source::Scan { table, alias } => Ok(format!("{} AS {}", ident(table), ident(alias))),
        Source::Join { left, right, on } => {
            if matches!(right.as_ref(), Source::Join { .. }) {
                return Err(NoSqlForm::RightNestedJoin);
            }
            let _ = is_root;
            let conditions: Vec<String> = on
                .iter()
                .map(|(l, r)| format!("{} = {}", column(l), column(r)))
                .collect();
            Ok(format!(
                "{} JOIN {} ON {}",
                source_sql(left, false)?,
                source_sql(right, false)?,
                conditions.join(" AND ")
            ))
        }
    }
}

/// Every identifier double-quoted, so a column called `count` or `order` is still a column.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A column reference: each dotted part quoted separately, so `a.c0` reads back as `a.c0`.
fn column(name: &str) -> String {
    name.split('.').map(ident).collect::<Vec<_>>().join(".")
}

fn expr_sql(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => column(name),
        Expr::Literal(value) => literal_sql(value),
        Expr::Null(ty) => format!("CAST(NULL AS {})", type_sql(*ty)),
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", expr_sql(left), op.name(), expr_sql(right))
        }
        Expr::Not(inner) => format!("(NOT {})", expr_sql(inner)),
        Expr::And(l, r) => format!("({} AND {})", expr_sql(l), expr_sql(r)),
        Expr::Or(l, r) => format!("({} OR {})", expr_sql(l), expr_sql(r)),
        Expr::IsNull(inner) => format!("({} IS NULL)", expr_sql(inner)),
        Expr::IsNotNull(inner) => format!("({} IS NOT NULL)", expr_sql(inner)),
        Expr::Case { whens, otherwise } => {
            let mut out = String::from("CASE");
            for (condition, result) in whens {
                let _ = write!(
                    out,
                    " WHEN {} THEN {}",
                    expr_sql(condition),
                    expr_sql(result)
                );
            }
            if let Some(else_expr) = otherwise {
                let _ = write!(out, " ELSE {}", expr_sql(else_expr));
            }
            out.push_str(" END");
            out
        }
    }
}

fn literal_sql(value: &Value) -> String {
    match value {
        Value::Int(i) => format!("{i}"),
        Value::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).to_owned(),
        // Neither can appear in a query that binds (S-3, S-19), so neither can be rendered. If one
        // ever does, this produces text that fails to parse rather than text that means something.
        Value::Float(x) => format!("/* unrenderable float {x} */"),
        Value::Null => "/* unrenderable untyped null */".to_owned(),
    }
}

fn type_sql(ty: DataType) -> &'static str {
    match ty {
        DataType::Int64 => "BIGINT",
        DataType::Utf8 => "TEXT",
        DataType::Boolean => "BOOLEAN",
        // Refused by the binder on both sides of the door (S-3), so unreachable from a bound query.
        DataType::Float64 => "DOUBLE",
    }
}

fn agg_sql(func: &AggFunc) -> String {
    match func {
        AggFunc::CountStar => "COUNT(*)".to_owned(),
        AggFunc::Count(e) => format!("COUNT({})", expr_sql(e)),
        AggFunc::Sum(e) => format!("SUM({})", expr_sql(e)),
        AggFunc::Min(e) => format!("MIN({})", expr_sql(e)),
        AggFunc::Max(e) => format!("MAX({})", expr_sql(e)),
        AggFunc::Avg(e) => format!("AVG({})", expr_sql(e)),
    }
}

/// Convenience for tests that want the rendered form of one projection item.
#[must_use]
pub fn item_sql(item: &Named<Expr>) -> String {
    format!("{} AS {}", expr_sql(&item.value), ident(&item.name))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_plan::plan::BinOp;

    #[test]
    fn a_filter_project_query_renders() {
        let query = Query::from(Source::scan("t0", "a"))
            .filter(Expr::binary(BinOp::Gt, Expr::column("a.c0"), Expr::int(-3)))
            .project(vec![Named::new("o0", Expr::column("a.c1"))]);
        assert_eq!(
            sql_form(&query).unwrap(),
            "SELECT \"a\".\"c1\" AS \"o0\" FROM \"t0\" AS \"a\" \
             WHERE (\"a\".\"c0\" > -3)"
        );
    }

    #[test]
    fn a_grouped_query_renders_keys_then_aggregates() {
        let query = Query::from(Source::scan("t0", "a")).group_by(GroupBy {
            keys: vec![Named::new("k0", Expr::column("a.c0"))],
            aggregates: vec![
                Named::new("g0", AggFunc::CountStar),
                Named::new("g1", AggFunc::Avg(Expr::column("a.c0"))),
            ],
            having: Some(Expr::binary(BinOp::Gt, Expr::column("g0"), Expr::int(1))),
        });
        assert_eq!(
            sql_form(&query).unwrap(),
            "SELECT \"a\".\"c0\" AS \"k0\", COUNT(*) AS \"g0\", AVG(\"a\".\"c0\") AS \"g1\" \
             FROM \"t0\" AS \"a\" GROUP BY \"a\".\"c0\" HAVING (\"g0\" > 1)"
        );
    }

    #[test]
    fn declines_are_specific() {
        let bare = Query::from(Source::scan("t0", "a"));
        assert_eq!(sql_form(&bare).unwrap_err(), NoSqlForm::WholeInputSchema);

        let grouped_and_projected = Query::from(Source::scan("t0", "a"))
            .group_by(GroupBy {
                keys: vec![Named::new("k0", Expr::column("a.c0"))],
                aggregates: vec![Named::new("g0", AggFunc::CountStar)],
                having: None,
            })
            .project(vec![Named::new("o0", Expr::column("k0"))]);
        assert_eq!(
            sql_form(&grouped_and_projected).unwrap_err(),
            NoSqlForm::ProjectionOverGroupBy
        );

        let duplicate_keys = Query::from(Source::scan("t0", "a")).group_by(GroupBy {
            keys: vec![
                Named::new("k0", Expr::column("a.c0")),
                Named::new("k1", Expr::column("a.c0")),
            ],
            aggregates: vec![Named::new("g0", AggFunc::CountStar)],
            having: None,
        });
        assert_eq!(
            sql_form(&duplicate_keys).unwrap_err(),
            NoSqlForm::DuplicateGroupKeyExpression
        );

        let right_nested = Query::from(Source::join(
            Source::scan("t0", "a"),
            Source::join(
                Source::scan("t1", "b"),
                Source::scan("t2", "c"),
                vec![("b.id".to_owned(), "c.id".to_owned())],
            ),
            vec![("a.id".to_owned(), "b.id".to_owned())],
        ))
        .project(vec![Named::new("o0", Expr::column("a.id"))]);
        assert_eq!(
            sql_form(&right_nested).unwrap_err(),
            NoSqlForm::RightNestedJoin
        );
    }

    /// A name that would collide with a keyword, or contain a quote, still round-trips.
    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(ident("count"), "\"count\"");
        assert_eq!(ident("od\"d"), "\"od\"\"d\"");
        assert_eq!(column("a.c0"), "\"a\".\"c0\"");
    }
}
