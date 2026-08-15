//! The binder: one `SELECT` → one [`schweep_plan::Query`] (`docs/SEMANTICS.md` S-9 … S-12, S-35).
//!
//! The output of this module is the **same** `Query` the typed API constructs by hand. That is the
//! whole design: SQL is a way of writing a query, not a second kind of query (S-35), and I-6 checks
//! it by compiling both doors and comparing the circuit plans structurally.
//!
//! ## The three decisions this file implements
//!
//! **Names (S-11).** A select item is named by its `AS`, or — for a bare column reference — by the
//! column's own unqualified name. Nothing else gets a name, because every derived name is a name the
//! user did not choose, and the output schema is part of the answer (S-8).
//!
//! **Grouping (S-27, S-33).** A query aggregates if it has a `GROUP BY`, or an aggregate in the
//! select list, or a `HAVING`. When it aggregates, every select item must be either an aggregate or
//! one of the grouping expressions — anything else belongs to no group and is refused. `GROUP BY`
//! with no keys at all is the grand total, and it exists whether or not the input has rows.
//!
//! **Projection (S-36).** A `GROUP BY` computes keys then aggregates, under their declared names. If
//! that is already what the select list asked for, in that order, no projection is emitted; if the
//! select list reorders or narrows, one is. This is the one place where the *shape* of the plan
//! depends on the text, which is why S-36 writes it down.

use schweep_plan::bind::{bind, Bound, Catalog};
use schweep_plan::plan::{AggFunc, Expr, GroupBy, Named, Query, Source};
use sqlparser::ast::{self, GroupByExpr, JoinConstraint, JoinOperator, SelectItem, TableFactor};

use crate::error::{Result, SqlError};
use crate::expr::{self, Context};
use crate::parse::SelectStatement;

/// A bound SQL query: the plan, and the schemas binding proved it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundQuery {
    pub query: Query,
    pub bound: Bound,
}

/// The select list, classified (S-30, S-33).
///
/// Two lists rather than one list of an either-type, because the two go to different places: scalars
/// become GROUP BY keys or a projection, aggregates become the aggregate list. `order` keeps what
/// both lose — the sequence the person actually wrote, which S-36 needs and which is part of the
/// answer's schema (S-8).
#[derive(Debug, Default)]
struct Items {
    scalars: Vec<Named<Expr>>,
    aggregates: Vec<Named<AggFunc>>,
    order: Vec<String>,
}

/// Bind one `SELECT` against a catalog.
pub fn bind_select(statement: &SelectStatement<'_>, catalog: &Catalog) -> Result<BoundQuery> {
    let select = statement.select;

    let source = bind_from(&select.from)?;

    let filter = match &select.selection {
        None => None,
        Some(predicate) => Some(expr::scalar(predicate, Context::Where)?),
    };

    let group_exprs = match &select.group_by {
        // Modifiers were refused in `parse`; only the expression list reaches here.
        GroupByExpr::Expressions(exprs, _) => exprs,
        GroupByExpr::All(_) => return Err(SqlError::NotInDialect("GROUP BY ALL")),
    };

    let items = bind_select_items(&select.projection)?;

    // A `HAVING` makes a query aggregate even with no `GROUP BY` and no aggregate in the select
    // list, because `HAVING` is defined over group output (S-32). Such a query has no keys and no
    // aggregates, so it computes nothing and `EmptyGroupKeys` says exactly that.
    let aggregating =
        !group_exprs.is_empty() || !items.aggregates.is_empty() || select.having.is_some();

    let query = if aggregating {
        bind_aggregating(statement, source, filter, group_exprs, items)?
    } else {
        // `aggregating` is false, so `items.aggregates` is empty and the scalars *are* the select
        // list. Nothing is dropped here, and the condition above is what proves it.
        Query {
            source,
            filter,
            group_by: None,
            project: Some(items.scalars),
            distinct: statement.distinct,
        }
    };

    let bound = bind(&query, catalog)?;
    Ok(BoundQuery { query, bound })
}

/// A query that groups (S-27, S-32, S-33, S-36).
fn bind_aggregating(
    statement: &SelectStatement<'_>,
    source: Source,
    filter: Option<Expr>,
    group_exprs: &[ast::Expr],
    items: Items,
) -> Result<Query> {
    // Each grouping expression, translated once, so that matching it against select items is an
    // equality on *plans* rather than on syntax. `GROUP BY t.a` and a select item `t.a` are the same
    // key even though the AST nodes differ in span.
    let mut keys: Vec<Named<Expr>> = Vec::with_capacity(group_exprs.len());
    let mut key_exprs: Vec<Expr> = Vec::with_capacity(group_exprs.len());
    for group_expr in group_exprs {
        let value = expr::scalar(group_expr, Context::Scalar)?;
        // The key's output name comes from the select item that asks for it, if any; otherwise it
        // is derived, which is possible only for a bare column reference (S-11).
        let name = match items.scalars.iter().find(|item| item.value == value) {
            Some(item) => item.name.clone(),
            None => derived_name(group_expr)?,
        };
        keys.push(Named::new(name, value.clone()));
        key_exprs.push(value);
    }

    // Every scalar select item must be one of the grouping expressions; anything else belongs to no
    // group (S-33). This is the refusal that dialects which "helpfully" pick an arbitrary row for
    // `SELECT t.a, COUNT(*) FROM t` do not make.
    for item in &items.scalars {
        if !key_exprs.contains(&item.value) {
            return Err(SqlError::Plan(schweep_plan::PlanError::ColumnNotGrouped {
                name: item.name.clone(),
            }));
        }
    }

    let aggregates = items.aggregates;

    let having = match &statement.select.having {
        None => None,
        Some(predicate) => Some(expr::scalar(predicate, Context::Having)?),
    };

    // The group output, in the order the operator produces it: keys, then aggregates (S-27).
    let group_output: Vec<&str> = keys
        .iter()
        .map(|k| k.name.as_str())
        .chain(aggregates.iter().map(|a| a.name.as_str()))
        .collect();
    let asked_for: Vec<&str> = items.order.iter().map(String::as_str).collect();

    // S-36: a projection to the schema you already have is a node that does nothing.
    let project = if asked_for == group_output {
        None
    } else {
        let mut project = Vec::with_capacity(items.order.len());
        for name in &items.order {
            // A scalar item reads the key it matched, which may carry a *different* name when two
            // items ask for one grouping expression under two aliases. An aggregate reads itself.
            let source_name = match items.scalars.iter().find(|item| &item.name == name) {
                Some(item) => key_exprs
                    .iter()
                    .position(|key| key == &item.value)
                    .and_then(|index| keys.get(index))
                    .map(|key| key.name.clone())
                    // Every scalar item was proved to be a grouping expression above.
                    .unwrap_or_else(|| name.clone()),
                None => name.clone(),
            };
            project.push(Named::new(name.clone(), Expr::Column(source_name)));
        }
        Some(project)
    };

    Ok(Query {
        source,
        filter,
        group_by: Some(GroupBy {
            keys,
            aggregates,
            having,
        }),
        project,
        distinct: statement.distinct,
    })
}

/// Classify and name every select item (S-11, S-30).
fn bind_select_items(projection: &[SelectItem]) -> Result<Items> {
    if projection.is_empty() {
        // `SELECT FROM t` parses in the generic dialect. A query with no output columns has an empty
        // schema, and an answer with no columns is not an answer.
        return Err(SqlError::NotInDialect("a SELECT with no output columns"));
    }
    let mut items = Items::default();
    for element in projection {
        let (e, name) = match element {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                return Err(SqlError::SelectStarNotSupported)
            }
            SelectItem::ExprWithAliases { .. } => {
                return Err(SqlError::NotInDialect(
                    "multiple aliases on one output column",
                ))
            }
            SelectItem::UnnamedExpr(e) => (e, derived_name(e)?),
            SelectItem::ExprWithAlias { expr: e, alias } => (e, alias.value.clone()),
        };
        items.order.push(name.clone());
        match expr::as_aggregate(e) {
            Some(function) => items
                .aggregates
                .push(Named::new(name, expr::aggregate(function)?)),
            None => items
                .scalars
                .push(Named::new(name, expr::scalar(e, Context::Scalar)?)),
        }
    }
    Ok(items)
}

/// The name an expression carries on its own (S-11): a column reference, and nothing else.
fn derived_name(e: &ast::Expr) -> Result<String> {
    match e {
        ast::Expr::Identifier(ident) => Ok(ident.value.clone()),
        ast::Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|part| part.value.clone())
            .ok_or_else(|| SqlError::MissingOutputName(e.to_string())),
        // Deliberately *not* recursing through `Nested`: `SELECT (t.a) FROM t` asked for an
        // expression, and an expression needs a name. Peeling parentheses to find a name would make
        // the rule depend on how many the person typed.
        _ => Err(SqlError::MissingOutputName(e.to_string())),
    }
}

/// `FROM` → [`Source`] (S-23, S-26).
fn bind_from(from: &[ast::TableWithJoins]) -> Result<Source> {
    let Some(first) = from.first() else {
        // Every query reads something: there is no `SELECT 1` without a FROM here, because a query
        // with no source has no deltas and so nothing to maintain.
        return Err(SqlError::NotInDialect("a SELECT with no FROM"));
    };
    if from.len() != 1 {
        return Err(SqlError::NotInDialect(
            "a comma join (write JOIN ... ON instead)",
        ));
    }

    let mut source = table_factor(&first.relation)?;
    for join in &first.joins {
        if join.global {
            return Err(SqlError::NotInDialect("GLOBAL JOIN"));
        }
        let constraint = match &join.join_operator {
            JoinOperator::Inner(c) | JoinOperator::Join(c) => c,
            JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => {
                return Err(SqlError::NotInDialect("LEFT JOIN"))
            }
            JoinOperator::Right(_) | JoinOperator::RightOuter(_) => {
                return Err(SqlError::NotInDialect("RIGHT JOIN"))
            }
            JoinOperator::FullOuter(_) => return Err(SqlError::NotInDialect("FULL OUTER JOIN")),
            JoinOperator::CrossJoin(_) | JoinOperator::CrossApply | JoinOperator::OuterApply => {
                return Err(SqlError::NotInDialect("CROSS JOIN"))
            }
            _ => return Err(SqlError::NotInDialect("that join kind")),
        };
        let on = match constraint {
            JoinConstraint::On(predicate) => predicate,
            JoinConstraint::Using(_) => return Err(SqlError::NotInDialect("JOIN USING")),
            JoinConstraint::Natural => return Err(SqlError::NotInDialect("NATURAL JOIN")),
            JoinConstraint::None => return Err(SqlError::NotInDialect("a join with no ON")),
        };
        let right = table_factor(&join.relation)?;
        let pairs = equi_pairs(on, &source, &right)?;
        source = Source::join(source, right, pairs);
    }
    Ok(source)
}

fn table_factor(factor: &TableFactor) -> Result<Source> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } => {
            if args.is_some() {
                return Err(SqlError::NotInDialect("a table-valued function"));
            }
            if !with_hints.is_empty() {
                return Err(SqlError::NotInDialect("table hints"));
            }
            if version.is_some() {
                return Err(SqlError::NotInDialect(
                    "table time-travel (FOR SYSTEM_TIME)",
                ));
            }
            if *with_ordinality {
                return Err(SqlError::NotInDialect("WITH ORDINALITY"));
            }
            if !partitions.is_empty() {
                return Err(SqlError::NotInDialect("PARTITION selection"));
            }
            if json_path.is_some() {
                return Err(SqlError::NotInDialect("a JSON path"));
            }
            if sample.is_some() {
                return Err(SqlError::NotInDialect("TABLESAMPLE"));
            }
            if !index_hints.is_empty() {
                return Err(SqlError::NotInDialect("index hints"));
            }

            // `name.to_string()` would re-render the quoting; the *value* is what names the table.
            let table = match name.0.as_slice() {
                [ast::ObjectNamePart::Identifier(ident)] => ident.value.clone(),
                _ => return Err(SqlError::QualifiedTableName(name.to_string())),
            };
            // `FROM t` makes the columns `t.a` — the table's own name is the alias when none is
            // written, which is what makes every column reference qualified (S-10) without
            // requiring an alias on every table.
            let alias_name = match alias {
                None => table.clone(),
                Some(table_alias) => {
                    if !table_alias.columns.is_empty() {
                        return Err(SqlError::NotInDialect("column aliases on a table"));
                    }
                    table_alias.name.value.clone()
                }
            };
            Ok(Source::scan(table, alias_name))
        }
        TableFactor::Derived { .. } => Err(SqlError::NotInDialect("a derived table (subquery)")),
        TableFactor::NestedJoin { .. } => Err(SqlError::NotInDialect("a parenthesised join")),
        TableFactor::TableFunction { .. } | TableFactor::Function { .. } => {
            Err(SqlError::NotInDialect("a table-valued function"))
        }
        _ => Err(SqlError::NotInDialect("that FROM item")),
    }
}

/// `ON a.x = b.y AND a.z = b.w` → the key pairs, each ordered left-side-first (S-26).
fn equi_pairs(on: &ast::Expr, left: &Source, right: &Source) -> Result<Vec<(String, String)>> {
    let mut conjuncts = Vec::new();
    flatten_and(on, &mut conjuncts);

    let left_aliases = left.aliases();
    let right_aliases = right.aliases();
    let mut pairs = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let ast::Expr::BinaryOp {
            left: l,
            op: ast::BinaryOperator::Eq,
            right: r,
        } = conjunct
        else {
            return Err(SqlError::NotAnEquiJoin(conjunct.to_string()));
        };
        let (l_name, r_name) = (column_name(l)?, column_name(r)?);
        let (l_side, r_side) = (qualifier(&l_name)?, qualifier(&r_name)?);

        let l_on_left = left_aliases.contains(&l_side);
        let r_on_right = right_aliases.contains(&r_side);
        let l_on_right = right_aliases.contains(&l_side);
        let r_on_left = left_aliases.contains(&r_side);

        if l_on_left && r_on_right {
            pairs.push((l_name, r_name));
        } else if l_on_right && r_on_left {
            // `ON b.y = a.x` means the same join as `ON a.x = b.y`; the operator needs the left
            // side first, so the pair is put in order here rather than refused.
            pairs.push((r_name, l_name));
        } else if (l_on_left && r_on_left) || (l_on_right && r_on_right) {
            return Err(SqlError::JoinKeysOnOneSide(l_name, r_name));
        } else {
            // An alias that belongs to neither side: the binder would report it as unknown, but
            // saying it is not an equi-join between *these* two relations is more precise.
            return Err(SqlError::NotAnEquiJoin(conjunct.to_string()));
        }
    }
    if pairs.is_empty() {
        return Err(SqlError::Plan(
            schweep_plan::PlanError::CrossJoinNotSupported,
        ));
    }
    Ok(pairs)
}

fn flatten_and<'a>(e: &'a ast::Expr, out: &mut Vec<&'a ast::Expr>) {
    match e {
        ast::Expr::BinaryOp {
            left,
            op: ast::BinaryOperator::And,
            right,
        } => {
            flatten_and(left, out);
            flatten_and(right, out);
        }
        ast::Expr::Nested(inner) => flatten_and(inner, out),
        other => out.push(other),
    }
}

fn column_name(e: &ast::Expr) -> Result<String> {
    match expr::scalar(e, Context::Scalar)? {
        Expr::Column(name) => Ok(name),
        _ => Err(SqlError::NotAnEquiJoin(e.to_string())),
    }
}

fn qualifier(name: &str) -> Result<&str> {
    match name.split_once('.') {
        Some((alias, _)) => Ok(alias),
        // An unqualified column in an `ON` cannot be attributed to a side, and guessing is exactly
        // what S-10 refuses to do.
        None => Err(SqlError::Plan(schweep_plan::PlanError::UnqualifiedColumn(
            name.to_owned(),
        ))),
    }
}
