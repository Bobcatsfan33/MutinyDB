//! The seeded scenario generator (`ARCHITECTURE.md` §6 C0, §7).
//!
//! A scenario is a pure function of its seed: the same seed produces the same tables, the same
//! query, and the same epoch-by-epoch deltas, byte for byte, forever. That is what makes a
//! failure reproducible from the number printed next to it.
//!
//! ## What the generator must always produce (§7)
//!
//! Retractions from epoch one · weight multiplicities greater than 1 · same-epoch retract-and-
//! insert of the same row · empty epochs · empty inputs. Every one of those is a named
//! [`Operation`] below, and [`coverage`] proves over a sample of seeds that each really occurs
//! rather than being theoretically reachable.
//!
//! **The generator defines the bar.** It is never weakened so that an implementation can pass.
//! In particular, retractions are here from epoch one and stay here — the C0 pitfall in §6 is
//! exactly the temptation to postpone them until the engine can cope, and postponing them would
//! mean the engine's handling of deletion went unmeasured for four sprints.
//!
//! ## What it deliberately does not produce
//!
//! Only what the rung-1–3 dialect contains: no DISTINCT, no UNION, no ORDER BY, no outer joins,
//! no subqueries (S-7 of the "does not define" table). Those arrive with their rungs.
//!
//! Malformed histories are also excluded: every retraction is checked against a model of the
//! table's current contents, so the generator never retracts a row that is not there. That is not
//! squeamishness — S-5 says such a history has no defined answer, so a scenario containing one
//! would be asking two implementations to agree about nothing. The oracle's `NegativeIntegral`
//! check is tested directly instead, in `schweep-oracle`'s own suite.
//!
//! ## Expressions that raise
//!
//! From C3 the generator **does** produce expressions that raise: division by a column that may be
//! zero, and occasional `i64::MAX` literals that make `+`, `-`, `*` and `SUM` overflow. Before C3 it
//! did not, because the two implementations disagreed about what an error meant to a standing query
//! and the gates had to assert that none occurred. D-16 settled that — an error is a property of the
//! contents (S-22) — so the gates now check the stronger claim: that both sides agree about *which*
//! epochs raise and *what they say*.

use std::collections::BTreeMap;

use schweep_plan::bind::{bind, bind_source, Catalog};
use schweep_plan::plan::{AggFunc, BinOp, Expr, GroupBy, Named, Query, Source};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

use crate::rng::Rng;

/// One generated scenario: everything two implementations need to be compared.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub tables: Vec<(String, Schema)>,
    pub query: Query,
    pub epochs: Vec<EpochDeltas>,
    /// Which shape the query has, for reporting and for coverage checks.
    pub family: Family,
}

/// The query shapes the generator builds, one per dialect rung plus their combination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// Rung 1: SELECT / WHERE / projection over one table.
    FilterProject,
    /// Rung 2: INNER equi-join, optionally filtered and projected.
    Join,
    /// Rung 3: GROUP BY with aggregates and optional HAVING.
    Aggregate,
    /// Rung 2 into rung 3: aggregate over a join, the shape that breaks things.
    JoinAggregate,
}

impl Family {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Family::FilterProject => "filter-project",
            Family::Join => "join",
            Family::Aggregate => "aggregate",
            Family::JoinAggregate => "join-aggregate",
        }
    }

    /// True if this family needs two tables.
    fn needs_two_tables(self) -> bool {
        matches!(self, Family::Join | Family::JoinAggregate)
    }
}

/// The kinds of change an epoch can carry. Named so that coverage can be asserted rather than
/// hoped for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operation {
    /// A row that was not present, at weight ≥ 1.
    InsertNew,
    /// More copies of a row that is already present — a weight multiplicity above 1.
    InsertDuplicate,
    /// Some, but not all, copies of a present row.
    RetractPartial,
    /// Every copy of a present row: the row leaves the table.
    RetractAll,
    /// A retraction and an insertion of *different* rows in one epoch — an update.
    UpdateInPlace,
    /// An insertion and its retraction of the *same* row in one epoch, netting to nothing.
    ChurnSameEpoch,
}

impl Scenario {
    /// Generate the scenario for a seed. Pure: same seed, same scenario, always.
    ///
    /// Fallible, and loudly so. Every failure here means the generator built something the
    /// binder rejects, which is a bug in the generator — the alternative, quietly falling back
    /// to a simpler query, would turn that bug into a silent loss of coverage that no test could
    /// see.
    pub fn generate(seed: u64) -> Result<Scenario, String> {
        let mut rng = Rng::from_seed(seed);
        let family = *rng
            .pick(&[
                Family::FilterProject,
                Family::Join,
                Family::Aggregate,
                Family::JoinAggregate,
            ])
            .unwrap_or(&Family::FilterProject);

        let table_count = if family.needs_two_tables() { 2 } else { 1 };
        let mut tables: Vec<(String, Schema)> = Vec::with_capacity(table_count);
        for i in 0..table_count {
            tables.push((format!("t{i}"), generate_table_schema(&mut rng)?));
        }

        let catalog: Catalog = tables.iter().cloned().collect();
        let query = generate_query(&mut rng, family, &tables, &catalog)?;
        let epochs = generate_epochs(&mut rng, &tables);

        Ok(Scenario {
            seed,
            tables,
            query,
            epochs,
            family,
        })
    }

    /// Which operations this scenario's epochs actually contain, recomputed from the deltas.
    ///
    /// Derived from the data rather than recorded during generation, so it describes what a
    /// consumer will really see. If the generator ever stops emitting a shape, this notices.
    #[must_use]
    pub fn operations(&self) -> Vec<Operation> {
        let mut seen: Vec<Operation> = Vec::new();
        let mut model: BTreeMap<String, BTreeMap<Row, i64>> = BTreeMap::new();
        for (name, _) in &self.tables {
            model.insert(name.clone(), BTreeMap::new());
        }

        for epoch in &self.epochs {
            for (table, entries) in epoch.tables() {
                let contents = model.entry(table.clone()).or_default();

                // Net effect per row within this epoch, to recognise churn and updates.
                let mut net: BTreeMap<&Row, i64> = BTreeMap::new();
                let mut touched_positively = false;
                let mut touched_negatively = false;
                for (row, weight) in entries {
                    *net.entry(row).or_insert(0) += *weight;
                    if *weight > 0 {
                        touched_positively = true;
                    }
                    if *weight < 0 {
                        touched_negatively = true;
                    }
                }

                for (row, delta) in &net {
                    let before = contents.get(*row).copied().unwrap_or(0);
                    let after = before + delta;
                    if before == 0 && *delta > 0 {
                        seen.push(Operation::InsertNew);
                        if *delta > 1 {
                            seen.push(Operation::InsertDuplicate);
                        }
                    } else if before > 0 && *delta > 0 {
                        seen.push(Operation::InsertDuplicate);
                    } else if *delta < 0 && after == 0 {
                        seen.push(Operation::RetractAll);
                    } else if *delta < 0 {
                        seen.push(Operation::RetractPartial);
                    } else if *delta == 0 && entries.iter().any(|(r, _)| r == *row) {
                        // Present in the epoch but netting to zero: inserted and retracted, or
                        // retracted and reinserted, within the same epoch.
                        seen.push(Operation::ChurnSameEpoch);
                    }
                }
                if touched_positively && touched_negatively {
                    seen.push(Operation::UpdateInPlace);
                }

                for (row, delta) in net {
                    let slot = contents.entry(row.clone()).or_insert(0);
                    *slot += delta;
                    if *slot == 0 {
                        contents.remove(row);
                    }
                }
            }
        }

        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// True if no epoch carries any change: the empty-input case (§7).
    #[must_use]
    pub fn is_empty_input(&self) -> bool {
        self.epochs.iter().all(EpochDeltas::is_empty)
    }

    /// True if at least one epoch is empty while others are not (§7).
    #[must_use]
    pub fn has_empty_epoch(&self) -> bool {
        self.epochs.iter().any(EpochDeltas::is_empty)
    }

    /// A complete, deterministic rendering of the scenario.
    ///
    /// Two calls for the same seed produce byte-identical strings, and that is the property the
    /// reproducibility gate asserts. It doubles as the failure report: everything needed to
    /// re-create a divergence by hand is in here.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "scenario seed={} family={} tables={} epochs={}\n",
            self.seed,
            self.family.name(),
            self.tables.len(),
            self.epochs.len()
        );
        for (name, schema) in &self.tables {
            out.push_str(&format!("table {name} {schema}\n"));
        }
        out.push_str(&format!("query {:#?}\n", self.query));
        for (index, epoch) in self.epochs.iter().enumerate() {
            out.push_str(&format!("epoch {}\n", index + 1));
            out.push_str(&epoch.render());
        }
        out
    }
}

/// The column every table has, and the column joins are built on.
///
/// Its values come from a deliberately tiny domain ([`KEY_DOMAIN`]) so that rows from two tables
/// actually match. With a wide key domain almost every join would return nothing, and a suite of
/// empty answers agrees with itself no matter how broken either side is.
const KEY_COLUMN: &str = "id";

/// The key domain: three values. Small enough that joins match and groups hold several rows.
const KEY_DOMAIN: (i64, i64) = (0, 2);

fn generate_table_schema(rng: &mut Rng) -> Result<Schema, String> {
    // Every table has an Int64 `id`, so a join key always exists and every family can be built
    // over any generated schema. It is nullable half the time — a null key never joins (S-26),
    // which is a behaviour worth generating, but not in every scenario.
    let mut fields = vec![Field::new(KEY_COLUMN, DataType::Int64, rng.chance(1, 2))];

    let extra = rng.below(3) + 1; // 1..=3 further columns
    let pool = [
        ("n", DataType::Int64),
        ("m", DataType::Int64),
        ("s", DataType::Utf8),
        ("f", DataType::Boolean),
    ];
    let mut used: Vec<&str> = vec!["id"];
    for _ in 0..extra {
        if let Some((name, ty)) = rng.pick(&pool) {
            if used.contains(name) {
                continue;
            }
            used.push(name);
            // Most columns are nullable: three-valued logic is the common case, not the exception
            // (S-13…S-18). A NOT NULL column now and then exercises the other branch.
            fields.push(Field::new(*name, *ty, rng.chance(4, 5)));
        }
    }

    // Names are unique by construction and no Float64 is in the pool, so this cannot fail —
    // but it is checked rather than assumed, because "cannot fail" is how unwraps are born.
    Schema::new_table(fields).map_err(|e| format!("generated an invalid table schema: {e}"))
}

// ---------------------------------------------------------------------------------------------
// Data generation
// ---------------------------------------------------------------------------------------------

/// Small value domains, on purpose.
///
/// Wide domains would make almost every generated row distinct, and then joins would rarely
/// match, groups would hold one row each, and consolidation would never merge anything. Nearly
/// every interesting behaviour in an incremental engine lives in collisions, so the generator
/// makes collisions likely.
fn generate_value(rng: &mut Rng, field: &Field) -> Value {
    if field.nullable && rng.chance(1, 4) {
        return Value::Null;
    }
    match field.data_type {
        DataType::Int64 if field.name == KEY_COLUMN => {
            Value::Int(rng.between(KEY_DOMAIN.0, KEY_DOMAIN.1))
        }
        DataType::Int64 => Value::Int(rng.between(-2, 3)),
        DataType::Utf8 => Value::Str((*rng.pick(&["", "a", "b", "cc"]).unwrap_or(&"a")).to_owned()),
        DataType::Boolean => Value::Bool(rng.chance(1, 2)),
        // Float64 cannot appear in a table schema (S-3); generated tables never contain one.
        DataType::Float64 => Value::Null,
    }
}

fn generate_row(rng: &mut Rng, schema: &Schema) -> Row {
    Row::new(
        schema
            .fields()
            .iter()
            .map(|f| generate_value(rng, f))
            .collect(),
    )
}

/// Build the epoch sequence, maintaining a model of each table's contents so that no retraction
/// ever removes something that is not there (S-5).
fn generate_epochs(rng: &mut Rng, tables: &[(String, Schema)]) -> Vec<EpochDeltas> {
    // One scenario in eight has no data at all: the empty-input case (§7).
    if rng.chance(1, 8) {
        let count = rng.below(3) + 1;
        return (0..count).map(|_| EpochDeltas::new()).collect();
    }

    let mut model: BTreeMap<String, BTreeMap<Row, i64>> = tables
        .iter()
        .map(|(name, _)| (name.clone(), BTreeMap::new()))
        .collect();

    let epoch_count = rng.below(7) + 2; // 2..=8
    let mut epochs = Vec::with_capacity(epoch_count as usize);

    for _ in 0..epoch_count {
        let mut input = EpochDeltas::new();
        // One epoch in six is empty: the answer must not move (§7).
        if !rng.chance(1, 6) {
            for (name, schema) in tables {
                // At least one operation per table per non-empty epoch. With zero permitted, a
                // scenario could seal eight epochs and still hold no data, and a run over an
                // empty table proves nothing about a join or an aggregate.
                let ops = rng.below(3) + 1; // 1..=3 operations per table per epoch
                for _ in 0..ops {
                    let contents = model.entry(name.clone()).or_default();
                    generate_operation(rng, name, schema, contents, &mut input);
                }
            }
        }
        epochs.push(input);
    }
    epochs
}

fn generate_operation(
    rng: &mut Rng,
    table: &str,
    schema: &Schema,
    contents: &mut BTreeMap<Row, i64>,
    input: &mut EpochDeltas,
) {
    let present: Vec<(Row, i64)> = contents.iter().map(|(r, w)| (r.clone(), *w)).collect();

    // With nothing present, the only honest move is an insertion — which is also why every
    // scenario starts by inserting and can retract from the very next operation onward.
    // The weighting is 3/8 insert, 1/8 duplicate, 2/8 retract, 1/8 update, 1/8 churn.
    // Insertions outnumber retractions so that tables actually accumulate rows: with the two
    // balanced, a table hovers near empty, joins find nothing to match, groups hold a single row,
    // and the suite spends its time comparing empty answers — which agree however broken either
    // side is. A quarter of all operations are still outright retractions, and update and churn
    // carry negative weights too. The bar in §6 C0 is that retractions are present from epoch
    // one; this keeps them ordinary, not rare.
    let choice = if present.is_empty() { 0 } else { rng.below(8) };

    match choice {
        // Insert a fresh row, sometimes at a weight above 1 (a multiplicity).
        0..=2 => {
            let row = generate_row(rng, schema);
            let weight = if rng.chance(1, 3) {
                rng.between(2, 3)
            } else {
                1
            };
            apply(contents, input, table, row, weight);
        }

        // More copies of something already present.
        3 => {
            if let Some(index) = rng.pick_index(present.len()) {
                if let Some((row, _)) = present.get(index) {
                    let weight = rng.between(1, 2);
                    apply(contents, input, table, row.clone(), weight);
                }
            }
        }

        // Retract some or all copies of a present row.
        4 | 5 => {
            if let Some(index) = rng.pick_index(present.len()) {
                if let Some((row, held)) = present.get(index) {
                    // Never more than is held: a larger retraction would be a malformed history
                    // (S-5), which has no defined answer to compare against.
                    let take = rng.between(1, *held);
                    apply(contents, input, table, row.clone(), -take);
                }
            }
        }

        // Update: retract a present row and insert a different one, in the same epoch. This is
        // the shape a real "UPDATE" takes in a Z-set — there is no update operator (S-4).
        6 => {
            if let Some(index) = rng.pick_index(present.len()) {
                if let Some((row, held)) = present.get(index) {
                    apply(contents, input, table, row.clone(), -*held);
                    let replacement = generate_row(rng, schema);
                    apply(contents, input, table, replacement, *held);
                }
            }
        }

        // Churn: insert a row and retract it again within one epoch, netting to nothing. The
        // answer must be identical to the epoch not having happened — which is a real bug class
        // in incremental engines, where the two halves can take different paths.
        _ => {
            let row = generate_row(rng, schema);
            let weight = rng.between(1, 2);
            apply(contents, input, table, row.clone(), weight);
            apply(contents, input, table, row, -weight);
        }
    }
}

fn apply(
    contents: &mut BTreeMap<Row, i64>,
    input: &mut EpochDeltas,
    table: &str,
    row: Row,
    weight: i64,
) {
    if weight == 0 {
        return;
    }
    input.push(table, row.clone(), weight);
    let slot = contents.entry(row.clone()).or_insert(0);
    *slot += weight;
    if *slot <= 0 {
        contents.remove(&row);
    }
}

// ---------------------------------------------------------------------------------------------
// Query generation
// ---------------------------------------------------------------------------------------------

/// Build a query of the given family over the generated tables.
///
/// The generator asks the oracle's own binder for the schema in scope at each stage, rather than
/// re-deriving the scoping rules. Re-deriving them would create a second implementation of S-10
/// and S-27 that could drift from the first, and a generator that disagrees with the binder
/// produces queries that fail to bind — noise that looks like a bug.
fn generate_query(
    rng: &mut Rng,
    family: Family,
    tables: &[(String, Schema)],
    catalog: &Catalog,
) -> Result<Query, String> {
    let source = match family {
        Family::FilterProject | Family::Aggregate => Source::scan(table_name(tables, 0), "a"),
        Family::Join | Family::JoinAggregate => Source::join(
            Source::scan(table_name(tables, 0), "a"),
            Source::scan(table_name(tables, 1), "b"),
            vec![("a.id".to_owned(), "b.id".to_owned())],
        ),
    };

    let input_schema = bind_source(&source, catalog)
        .map_err(|e| format!("generated a source that does not bind: {e}"))?;

    let mut query = Query::from(source);

    // WHERE, two times in three. Depth 1 rather than 2: each extra level of AND roughly halves
    // the rows that survive, and a filter that admits nothing turns the scenario into an
    // expensive way of comparing two empty answers.
    if rng.chance(2, 3) {
        let depth = if rng.chance(1, 4) { 2 } else { 1 };
        query = query.filter(generate_predicate(rng, &input_schema, depth));
    }

    let grouping = matches!(family, Family::Aggregate | Family::JoinAggregate);
    if grouping {
        let group_by = generate_group_by(rng, &input_schema);
        query = query.group_by(group_by);
    }

    // The projection is generated against whatever scope the query now presents — grouped or
    // not — which the binder is the authority on.
    let scope = bind(&query, catalog)
        .map_err(|e| format!("generated a query that does not bind: {e}"))?
        .grouped_schema;

    // HAVING, over the grouped scope, for two grouped queries in five.
    if grouping && rng.chance(2, 5) {
        if let Some(GroupBy {
            keys,
            aggregates,
            having: _,
        }) = query.group_by.clone()
        {
            let having = generate_predicate(rng, &scope, 1);
            query = query.group_by(GroupBy {
                keys,
                aggregates,
                having: Some(having),
            });
        }
    }

    // SELECT, half the time.
    if rng.chance(1, 2) {
        let count = rng.below(3) + 1;
        let mut items = Vec::with_capacity(count as usize);
        for index in 0..count {
            let expr = generate_expr_of_any_type(rng, &scope, 2);
            items.push(Named::new(format!("o{index}"), expr));
        }
        query = query.project(items);
    }

    // DISTINCT, one query in four (S-34). Applied last, after the projection, and it changes no
    // schema — only weights — so it needs no re-binding.
    if rng.chance(1, 4) {
        query = query.distinct();
    }

    // Bind the finished query once more: a query that leaves this function must bind, and
    // proving it here means a bind failure downstream is a bug in the *binder*, not the
    // generator. Nothing else in the harness has to wonder which it was.
    bind(&query, catalog).map_err(|e| format!("generated a query that does not bind: {e}"))?;
    Ok(query)
}

fn table_name(tables: &[(String, Schema)], index: usize) -> String {
    tables
        .get(index)
        .map_or_else(|| "t0".to_owned(), |(name, _)| name.clone())
}

fn generate_group_by(rng: &mut Rng, scope: &Schema) -> GroupBy {
    let key_count = rng.below(2) + 1; // 1..=2 keys
    let mut keys = Vec::with_capacity(key_count as usize);
    for index in 0..key_count {
        // Group by a bare column most of the time, by a derived expression sometimes — the
        // latter is where a key can be NULL for reasons the data does not show directly (S-28).
        let expr = if rng.chance(3, 4) {
            columns_of_any_type(scope)
                .first()
                .map_or_else(|| Expr::int(0), |f| Expr::column(&f.name))
        } else {
            generate_expr_of_any_type(rng, scope, 1)
        };
        let expr = if rng.chance(3, 4) {
            pick_column_expr(rng, scope).unwrap_or(expr)
        } else {
            expr
        };
        keys.push(Named::new(format!("k{index}"), expr));
    }

    let agg_count = rng.below(4) + 1; // 1..=4 aggregates
    let mut aggregates = Vec::with_capacity(agg_count as usize);
    for index in 0..agg_count {
        aggregates.push(Named::new(
            format!("g{index}"),
            generate_aggregate(rng, scope),
        ));
    }

    GroupBy {
        keys,
        aggregates,
        having: None,
    }
}

fn generate_aggregate(rng: &mut Rng, scope: &Schema) -> AggFunc {
    let int_col = pick_column_of(rng, scope, DataType::Int64);
    let any_col = pick_column_expr(rng, scope);

    match rng.below(6) {
        0 => AggFunc::CountStar,
        1 => any_col.map_or(AggFunc::CountStar, AggFunc::Count),
        2 => int_col.clone().map_or(AggFunc::CountStar, AggFunc::Sum),
        3 => int_col.map_or(AggFunc::CountStar, AggFunc::Avg),
        4 => any_col.map_or(AggFunc::CountStar, AggFunc::Min),
        _ => any_col.map_or(AggFunc::CountStar, AggFunc::Max),
    }
}

fn columns_of_any_type(scope: &Schema) -> Vec<&Field> {
    scope
        .fields()
        .iter()
        .filter(|f| f.data_type != DataType::Float64)
        .collect()
}

fn columns_of(scope: &Schema, ty: DataType) -> Vec<&Field> {
    scope
        .fields()
        .iter()
        .filter(|f| f.data_type == ty)
        .collect()
}

fn pick_column_expr(rng: &mut Rng, scope: &Schema) -> Option<Expr> {
    let candidates = columns_of_any_type(scope);
    rng.pick(&candidates).map(|f| Expr::column(&f.name))
}

fn pick_column_of(rng: &mut Rng, scope: &Schema, ty: DataType) -> Option<Expr> {
    let candidates = columns_of(scope, ty);
    rng.pick(&candidates).map(|f| Expr::column(&f.name))
}

/// A Boolean expression, three-valued (S-15…S-17).
fn generate_predicate(rng: &mut Rng, scope: &Schema, depth: u32) -> Expr {
    if depth == 0 {
        return generate_comparison(rng, scope);
    }
    // `OR` is generated more often than `AND`, and `NOT` least often. This is not a statement
    // about which is more interesting — it is about survival: each `AND` roughly halves the rows
    // that pass, and a predicate that admits nothing makes the whole scenario compare two empty
    // answers, which agree no matter how broken either side is. Nesting still happens; it is
    // just not the default shape.
    match rng.below(8) {
        0 => Expr::and(
            generate_predicate(rng, scope, depth - 1),
            generate_predicate(rng, scope, depth - 1),
        ),
        1 | 2 => Expr::or(
            generate_predicate(rng, scope, depth - 1),
            generate_predicate(rng, scope, depth - 1),
        ),
        3 => !generate_predicate(rng, scope, depth - 1),
        4 => pick_column_expr(rng, scope).map_or_else(
            || Expr::boolean(true),
            |c| {
                if rng.chance(1, 2) {
                    Expr::is_null(c)
                } else {
                    Expr::is_not_null(c)
                }
            },
        ),
        _ => generate_comparison(rng, scope),
    }
}

/// A comparison, either against a literal or between two columns of the same type.
fn generate_comparison(rng: &mut Rng, scope: &Schema) -> Expr {
    // Equality appears once and the inequalities twice each. Over a domain of a handful of
    // values, `= literal` admits roughly one row in six while `<= literal` admits about half;
    // weighting toward the inequalities keeps generated answers non-empty often enough to be
    // worth comparing, without removing equality — which joins and group keys need anyway.
    let op = *rng
        .pick(&[
            BinOp::Eq,
            BinOp::Ne,
            BinOp::Ne,
            BinOp::Lt,
            BinOp::Lt,
            BinOp::Le,
            BinOp::Le,
            BinOp::Gt,
            BinOp::Gt,
            BinOp::Ge,
            BinOp::Ge,
        ])
        .unwrap_or(&BinOp::Eq);

    let ty = *rng
        .pick(&[DataType::Int64, DataType::Utf8, DataType::Boolean])
        .unwrap_or(&DataType::Int64);

    let candidates = columns_of(scope, ty);
    let Some(left) = rng.pick(&candidates).map(|f| Expr::column(&f.name)) else {
        // No column of that type in scope: compare two literals rather than give up, so the
        // predicate is still well-typed and the query still binds.
        return Expr::binary(op, literal_of(rng, ty), literal_of(rng, ty));
    };

    // Sometimes compare to another column of the same type, sometimes to a literal, and once in
    // a while to a typed NULL — which makes the whole comparison NULL (S-13) and so exercises the
    // "neither matches nor complements" behaviour of S-17.
    // Comparing to a typed NULL makes the whole comparison NULL (S-13), so the predicate admits
    // nothing at all — worth generating, because "neither matches nor complements" (S-17) is a
    // real behaviour, but rarely, because as a top-level filter it empties the scenario.
    let right = if rng.chance(1, 16) {
        Expr::Null(ty)
    } else if rng.chance(1, 3) {
        rng.pick(&candidates)
            .map_or_else(|| literal_of(rng, ty), |f| Expr::column(&f.name))
    } else {
        literal_of(rng, ty)
    };

    Expr::binary(op, left, right)
}

fn literal_of(rng: &mut Rng, ty: DataType) -> Expr {
    match ty {
        // One integer literal in 32 is `i64::MAX`, so that `+`, `-` and `*` can overflow (S-20) and
        // `SUM` can exceed the Int64 range (S-30). Two *kinds* of live error in the population is
        // what exercises S-22c's "least message wins" rule, which a single kind never would.
        DataType::Int64 if rng.chance(1, 12) => Expr::int(i64::MAX),
        DataType::Int64 => Expr::int(rng.between(-2, 3)),
        DataType::Utf8 => Expr::string(*rng.pick(&["", "a", "b", "cc"]).unwrap_or(&"a")),
        DataType::Boolean => Expr::boolean(rng.chance(1, 2)),
        // Never generated: Float64 is result-only (S-3).
        DataType::Float64 => Expr::int(0),
    }
}

fn generate_expr_of_any_type(rng: &mut Rng, scope: &Schema, depth: u32) -> Expr {
    let ty = *rng
        .pick(&[
            DataType::Int64,
            DataType::Int64,
            DataType::Utf8,
            DataType::Boolean,
        ])
        .unwrap_or(&DataType::Int64);
    generate_expr(rng, scope, ty, depth)
}

/// An expression of a stated type. Every branch produces that type exactly — there is no
/// inference and no coercion to fall back on (S-19).
fn generate_expr(rng: &mut Rng, scope: &Schema, ty: DataType, depth: u32) -> Expr {
    if ty == DataType::Boolean {
        return generate_predicate(rng, scope, depth.min(1));
    }
    if depth == 0 {
        return pick_column_of(rng, scope, ty).unwrap_or_else(|| literal_of(rng, ty));
    }

    match ty {
        DataType::Int64 => match rng.below(8) {
            0..=2 => pick_column_of(rng, scope, ty).unwrap_or_else(|| literal_of(rng, ty)),
            3 => literal_of(rng, ty),
            4 | 5 => {
                let op = *rng
                    .pick(&[BinOp::Add, BinOp::Sub, BinOp::Mul])
                    .unwrap_or(&BinOp::Add);
                Expr::binary(
                    op,
                    generate_expr(rng, scope, ty, depth - 1),
                    generate_expr(rng, scope, ty, depth - 1),
                )
            }
            6 => {
                // Division by a non-zero literal most of the time, and by a *column* one time in
                // three — which can be zero or null, so it can raise `DivisionByZero` (S-21).
                //
                // Until C3 this generator deliberately never raised, because the two
                // implementations disagreed about what an error meant and the gates had to assert
                // that none occurred. D-16 settled that (an error is a property of the contents),
                // so raising expressions belong in the population now: the gates check that both
                // sides agree about *which* epochs raise and *what they say*, which is a stronger
                // claim than "neither raised".
                let divisor = if rng.chance(2, 3) {
                    pick_column_of(rng, scope, DataType::Int64)
                        .unwrap_or_else(|| Expr::int(*rng.pick(&[-3_i64, -1, 1, 3]).unwrap_or(&1)))
                } else {
                    Expr::int(*rng.pick(&[-3_i64, -2, -1, 1, 2, 3]).unwrap_or(&1))
                };
                Expr::binary(
                    BinOp::Div,
                    generate_expr(rng, scope, ty, depth - 1),
                    divisor,
                )
            }
            _ => Expr::Case {
                whens: vec![(
                    generate_predicate(rng, scope, 1),
                    generate_expr(rng, scope, ty, depth - 1),
                )],
                otherwise: if rng.chance(2, 3) {
                    Some(Box::new(generate_expr(rng, scope, ty, depth - 1)))
                } else {
                    // No ELSE: the result is NULL when nothing matches (S-18), which puts nulls
                    // into projections and group keys that the data itself never contained.
                    None
                },
            },
        },
        DataType::Utf8 | DataType::Boolean | DataType::Float64 => match rng.below(4) {
            0 => literal_of(rng, ty),
            1 if rng.chance(1, 3) => Expr::Null(ty),
            _ => pick_column_of(rng, scope, ty).unwrap_or_else(|| literal_of(rng, ty)),
        },
    }
}
