//! Binding: resolve every column, type every expression, refuse everything else by name (S-12).
//!
//! Binding is **total**. It either produces the query's output schema — having proved that every
//! column reference resolves, every operator's operands are the types it accepts, and every
//! construct is inside the dialect — or it fails with an error that names what was wrong. A query
//! is never partly bound and never silently coerced (S-19).
//!
//! Binding happens once per query, before any data is touched, so a badly-typed query fails the
//! same way on an empty database as on a full one.

use std::collections::BTreeMap;

use schweep_zset::{DataType, Field, Schema, Value};

use crate::error::{PlanError, Result};
use crate::plan::{AggFunc, Expr, GroupBy, Named, Query, Source};

/// Table name → schema.
pub type Catalog = BTreeMap<String, Schema>;

/// How columns are named in the current scope (S-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming {
    /// Before a GROUP BY: every column is `alias.column`.
    Qualified,
    /// After a GROUP BY: the only columns are the declared output names, unqualified (S-27).
    Unqualified,
}

/// The set of columns an expression may refer to, and how they must be written.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub schema: &'a Schema,
    pub naming: Naming,
}

impl<'a> Scope<'a> {
    pub fn new(schema: &'a Schema, naming: Naming) -> Scope<'a> {
        Scope { schema, naming }
    }

    /// Resolve a column name, enforcing the naming convention of this scope before looking it up
    /// — so that `k` before a GROUP BY is reported as *unqualified*, not as *unknown*, which is
    /// the difference between a useful error and a confusing one.
    pub fn resolve(&self, name: &str) -> Result<&'a Field> {
        match self.naming {
            Naming::Qualified if !name.contains('.') => {
                return Err(PlanError::UnqualifiedColumn(name.to_owned()));
            }
            Naming::Unqualified if name.contains('.') => {
                return Err(PlanError::QualifiedColumnAfterGroupBy(name.to_owned()));
            }
            _ => {}
        }
        let index = self
            .schema
            .index_of(name)
            .ok_or_else(|| PlanError::UnknownColumn {
                name: name.to_owned(),
                scope: self.schema.to_string(),
            })?;
        self.schema
            .field(index)
            .ok_or_else(|| PlanError::UnknownColumn {
                name: name.to_owned(),
                scope: self.schema.to_string(),
            })
    }
}

/// What binding a query produces: the schema of its answer.
///
/// The oracle evaluates the original [`Query`] against this schema rather than a rewritten plan.
/// That is deliberate: a second, "optimised" representation would be a second thing to be wrong,
/// and the oracle's whole value is that there is nothing in it to be wrong about (§5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    /// Columns visible to `WHERE`, to GROUP BY keys, and to aggregate arguments.
    pub input_schema: Schema,
    /// Columns visible to `HAVING` and to the projection: the GROUP BY output if the query
    /// groups, otherwise the input schema.
    pub grouped_schema: Schema,
    /// The schema of the answer.
    pub output_schema: Schema,
}

/// Bind a query against a catalog (S-12).
pub fn bind(query: &Query, catalog: &Catalog) -> Result<Bound> {
    // Aliases first: `FROM t JOIN t ON ...` would otherwise fail inside `bind_source`, where the
    // duplicate surfaces as two columns called `t.id` rather than as the one thing that is actually
    // wrong. A refusal must name its construct (S-12), and the construct here is the alias.
    check_aliases_unique(&query.source)?;
    let input_schema = bind_source(&query.source, catalog)?;

    let input_scope = Scope::new(&input_schema, Naming::Qualified);
    if let Some(predicate) = &query.filter {
        expect_boolean(predicate, input_scope, "WHERE")?;
    }

    let grouped_schema = match &query.group_by {
        None => input_schema.clone(),
        Some(group_by) => bind_group_by(group_by, input_scope)?,
    };

    let grouped_naming = if query.group_by.is_some() {
        Naming::Unqualified
    } else {
        Naming::Qualified
    };
    let grouped_scope = Scope::new(&grouped_schema, grouped_naming);

    if let Some(group_by) = &query.group_by {
        if let Some(having) = &group_by.having {
            expect_boolean(having, grouped_scope, "HAVING")?;
        }
    }

    let output_schema = match &query.project {
        None => grouped_schema.clone(),
        Some(items) => projection_schema(grouped_scope, items)?,
    };

    Ok(Bound {
        input_schema,
        grouped_schema,
        output_schema,
    })
}

/// The schema a source presents, with every column qualified by its alias (S-10, S-23, S-26).
pub fn bind_source(source: &Source, catalog: &Catalog) -> Result<Schema> {
    match source {
        Source::Scan { table, alias } => {
            let schema = catalog
                .get(table)
                .ok_or_else(|| PlanError::UnknownTable(table.clone()))?;
            let fields = schema
                .fields()
                .iter()
                .map(|f| Field::new(format!("{alias}.{}", f.name), f.data_type, f.nullable))
                .collect();
            Ok(Schema::new(fields)?)
        }
        Source::Join { left, right, on } => {
            if on.is_empty() {
                return Err(PlanError::CrossJoinNotSupported);
            }
            let left_schema = bind_source(left, catalog)?;
            let right_schema = bind_source(right, catalog)?;

            // Each key pair joins one column of the left to one column of the right, and the two
            // must have the same type — an equi-join across types would need a coercion, and
            // there are none (S-19).
            let left_scope = Scope::new(&left_schema, Naming::Qualified);
            let right_scope = Scope::new(&right_schema, Naming::Qualified);
            for (l, r) in on {
                let lf = left_scope.resolve(l)?;
                let rf = right_scope.resolve(r)?;
                if lf.data_type != rf.data_type {
                    return Err(PlanError::TypeMismatch {
                        op: "join key =",
                        left: lf.data_type,
                        right: rf.data_type,
                    });
                }
            }

            let mut fields: Vec<Field> = left_schema.fields().to_vec();
            fields.extend(right_schema.fields().iter().cloned());
            Ok(Schema::new(fields)?)
        }
    }
}

fn check_aliases_unique(source: &Source) -> Result<()> {
    let aliases = source.aliases();
    for (i, alias) in aliases.iter().enumerate() {
        if aliases.get(..i).is_some_and(|prior| prior.contains(alias)) {
            return Err(PlanError::DuplicateAlias((*alias).to_owned()));
        }
    }
    Ok(())
}

fn bind_group_by(group_by: &GroupBy, input: Scope<'_>) -> Result<Schema> {
    // Zero keys is the grand total, and it is legal: one group, always present (S-33, D-20). It used
    // to be refused as `EmptyGroupKeys` while the question was open.
    if group_by.aggregates.is_empty() {
        return Err(PlanError::EmptyGroupKeys);
    }
    let mut fields = Vec::with_capacity(group_by.keys.len() + group_by.aggregates.len());
    for key in &group_by.keys {
        let ty = type_of(&key.value, input)?;
        fields.push(output_field(&key.name, ty));
    }
    for agg in &group_by.aggregates {
        let ty = aggregate_result_type(&agg.value, input)?;
        fields.push(output_field(&agg.name, ty));
    }
    build_output_schema(fields)
}

/// The schema a projection produces over `scope` (S-11): the declared names, the types the
/// expressions have, every column nullable.
///
/// Public because the `Project` operator in `schweep-ops` needs exactly this schema and must not
/// compute it a second way. Two implementations of S-11 could disagree, and a disagreement about
/// an *answer's schema* is a disagreement about the answer (S-8) — it would surface as an I-1
/// failure whose cause is two copies of one rule, not a bug in either engine.
pub fn projection_schema(scope: Scope<'_>, items: &[Named<Expr>]) -> Result<Schema> {
    let mut fields = Vec::with_capacity(items.len());
    for item in items {
        let ty = type_of(&item.value, scope)?;
        fields.push(output_field(&item.name, ty));
    }
    build_output_schema(fields)
}

/// The result type of an aggregate, having checked that it accepts its argument's type (S-30).
pub fn aggregate_result_type(func: &AggFunc, scope: Scope<'_>) -> Result<DataType> {
    let name = func.name();
    match func {
        AggFunc::CountStar => Ok(DataType::Int64),
        AggFunc::Count(arg) => {
            // COUNT accepts any type: it counts presence, not value.
            type_of(arg, scope)?;
            Ok(DataType::Int64)
        }
        AggFunc::Sum(arg) => match type_of(arg, scope)? {
            DataType::Int64 => Ok(DataType::Int64),
            ty => Err(PlanError::AggregateTypeUnsupported { func: name, ty }),
        },
        AggFunc::Avg(arg) => match type_of(arg, scope)? {
            // The only source of a Float64 in the entire system (S-3, S-31).
            DataType::Int64 => Ok(DataType::Float64),
            ty => Err(PlanError::AggregateTypeUnsupported { func: name, ty }),
        },
        AggFunc::Min(arg) | AggFunc::Max(arg) => match type_of(arg, scope)? {
            ty @ (DataType::Int64 | DataType::Utf8 | DataType::Boolean) => Ok(ty),
            ty => Err(PlanError::AggregateTypeUnsupported { func: name, ty }),
        },
    }
}

/// Every output column is declared nullable (S-11).
fn output_field(name: &str, ty: DataType) -> Field {
    Field::nullable(name, ty)
}

fn build_output_schema(fields: Vec<Field>) -> Result<Schema> {
    // Translate the Z-set layer's duplicate-name error into one that names the rule.
    for (i, f) in fields.iter().enumerate() {
        if fields
            .get(..i)
            .is_some_and(|prior| prior.iter().any(|p| p.name == f.name))
        {
            return Err(PlanError::DuplicateOutputName(f.name.clone()));
        }
    }
    Ok(Schema::new(fields)?)
}

fn expect_boolean(expr: &Expr, scope: Scope<'_>, context: &'static str) -> Result<()> {
    match type_of(expr, scope)? {
        DataType::Boolean => Ok(()),
        found => Err(PlanError::ExpectedBoolean { context, found }),
    }
}

/// The type of an expression in a scope (S-19). Every expression has exactly one type; there is
/// no inference and no untyped null.
pub fn type_of(expr: &Expr, scope: Scope<'_>) -> Result<DataType> {
    match expr {
        Expr::Column(name) => Ok(scope.resolve(name)?.data_type),

        Expr::Literal(Value::Null) => Err(PlanError::UntypedNullLiteral),
        Expr::Literal(Value::Float(_)) => Err(PlanError::NotInDialect("a FLOAT literal")),
        Expr::Literal(v) => v
            .data_type()
            // `data_type()` is `None` only for `Null`, matched above.
            .ok_or(PlanError::UntypedNullLiteral),

        Expr::Null(DataType::Float64) => Err(PlanError::NotInDialect("a FLOAT null")),
        Expr::Null(ty) => Ok(*ty),

        Expr::Binary { op, left, right } => {
            let lt = type_of(left, scope)?;
            let rt = type_of(right, scope)?;
            if op.is_arithmetic() {
                if lt == DataType::Int64 && rt == DataType::Int64 {
                    Ok(DataType::Int64)
                } else {
                    Err(PlanError::TypeMismatch {
                        op: op.name(),
                        left: lt,
                        right: rt,
                    })
                }
            } else if lt == rt && matches!(lt, DataType::Int64 | DataType::Utf8 | DataType::Boolean)
            {
                Ok(DataType::Boolean)
            } else {
                Err(PlanError::TypeMismatch {
                    op: op.name(),
                    left: lt,
                    right: rt,
                })
            }
        }

        Expr::Not(inner) => match type_of(inner, scope)? {
            DataType::Boolean => Ok(DataType::Boolean),
            found => Err(PlanError::UnaryTypeMismatch { op: "NOT", found }),
        },

        Expr::And(left, right) | Expr::Or(left, right) => {
            let op = if matches!(expr, Expr::And(_, _)) {
                "AND"
            } else {
                "OR"
            };
            let lt = type_of(left, scope)?;
            let rt = type_of(right, scope)?;
            if lt == DataType::Boolean && rt == DataType::Boolean {
                Ok(DataType::Boolean)
            } else {
                Err(PlanError::TypeMismatch {
                    op,
                    left: lt,
                    right: rt,
                })
            }
        }

        // Always two-valued, whatever the operand is (S-16).
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            type_of(inner, scope)?;
            Ok(DataType::Boolean)
        }

        Expr::Case { whens, otherwise } => {
            let mut branch_type: Option<DataType> = None;
            if whens.is_empty() {
                return Err(PlanError::EmptyCase);
            }
            for (condition, result) in whens {
                expect_boolean(condition, scope, "a CASE condition")?;
                let ty = type_of(result, scope)?;
                match branch_type {
                    None => branch_type = Some(ty),
                    Some(expected) if expected == ty => {}
                    Some(expected) => {
                        return Err(PlanError::CaseBranchTypeMismatch {
                            expected,
                            found: ty,
                        })
                    }
                }
            }
            if let Some(else_expr) = otherwise {
                let ty = type_of(else_expr, scope)?;
                match branch_type {
                    Some(expected) if expected == ty => {}
                    Some(expected) => {
                        return Err(PlanError::CaseBranchTypeMismatch {
                            expected,
                            found: ty,
                        })
                    }
                    None => branch_type = Some(ty),
                }
            }
            branch_type.ok_or(PlanError::EmptyCase)
        }
    }
}

/// Convenience for the harness and tests: the declared names of a projection or group output.
pub fn names<T>(items: &[Named<T>]) -> Vec<&str> {
    items.iter().map(|i| i.name.as_str()).collect()
}
