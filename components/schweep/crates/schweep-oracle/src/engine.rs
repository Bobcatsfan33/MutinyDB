//! The naive reference engine (`ARCHITECTURE.md` §5.1).
//!
//! Tables are `Vec<Row>`. Epochs are replayed prefixes of the log. Every query is recomputed from
//! scratch, over the full input, at every epoch. There are no indexes, no incrementality, and no
//! cleverness — and there is not going to be any. This is the most important crate in the
//! repository and the one place where "slow and obvious" is the style guide.
//!
//! ## Why "replayed prefixes" and not an accumulated integral
//!
//! [`Oracle::contents_at`] recomputes a table's contents by folding *every* delta from epoch 1
//! forward, every time it is asked. Keeping a running integral would be faster and would also be
//! a small piece of incremental state — the exact thing the engine under test is allowed to have
//! and the oracle is not. An oracle that maintains state can share a bug with the engine it is
//! checking. This one cannot: its answer at epoch N depends on nothing but the log prefix, which
//! is I-2 stated as an implementation.
//!
//! ## Cost, stated plainly
//!
//! Answering at epoch N is O(N × data) for the replay, and the join is a nested loop, so a join
//! is O(|A| × |B|). Over a scenario of E epochs that is O(E² × data). This is intended and is
//! why scenario sizes in the harness are small. No performance claim is made for this crate and
//! none ever will be (I-10).

use std::collections::BTreeMap;

use schweep_plan::bind::{bind, Catalog};
use schweep_plan::eval::{eval, is_true};
use schweep_plan::plan::{Query, Source};
use schweep_zset::{Canonical, EpochDeltas, Row, Schema, Value, ZSetBatch};

use crate::aggregate;
use crate::error::{OracleError, Result};
use crate::live_errors::LiveErrors;

/// Epochs are dense integers starting at 1 (S-6). Epoch 0 means "nothing has been sealed".
pub type Epoch = u64;

/// A relation in flight: a schema and its entries. Not public — the oracle's intermediate stages
/// are an implementation detail, and the only thing anyone outside sees is the answer.
struct Relation {
    schema: Schema,
    entries: Vec<(Row, i64)>,
}

/// The naive reference engine.
#[derive(Clone, Debug)]
pub struct Oracle {
    catalog: Catalog,
    log: Vec<EpochDeltas>,
}

impl Oracle {
    /// Create an oracle over a set of named tables.
    ///
    /// Table schemas are checked as *storable* (S-3): no `Float64` columns, because `Float64` is
    /// a result-only type and no data ever arrives as one.
    pub fn new(tables: impl IntoIterator<Item = (String, Schema)>) -> Result<Oracle> {
        let mut catalog = Catalog::new();
        for (name, schema) in tables {
            // Re-validate through the table door so a Float64 column is caught here rather than
            // becoming a mysterious mismatch later.
            let checked = Schema::new_table(schema.fields().to_vec())?;
            if catalog.insert(name.clone(), checked).is_some() {
                return Err(OracleError::DuplicateTable(name));
            }
        }
        Ok(Oracle {
            catalog,
            log: Vec::new(),
        })
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The highest sealed epoch; 0 before anything is sealed.
    #[must_use]
    pub fn sealed_epoch(&self) -> Epoch {
        self.log.len() as Epoch
    }

    /// Seal one epoch (S-6) and return its number.
    ///
    /// Validates that every named table exists and that every value conforms to its column, then
    /// checks that no table's contents went negative (S-5, D-12) — a retraction of something that
    /// was never there is a malformed history, and it is reported here, at the epoch that caused
    /// it, rather than surfacing later as a strange answer.
    pub fn seal_epoch(&mut self, deltas: EpochDeltas) -> Result<Epoch> {
        for (table, entries) in deltas.tables() {
            let schema = self
                .catalog
                .get(table)
                .ok_or_else(|| OracleError::unknown_table(table.clone()))?;
            for (row, _) in entries {
                if row.len() != schema.len() {
                    return Err(OracleError::ZSet(schweep_zset::ZSetError::ArityMismatch {
                        expected: schema.len(),
                        found: row.len(),
                    }));
                }
                for (index, value) in row.values().iter().enumerate() {
                    schema.check_value(index, value)?;
                }
            }
        }

        self.log.push(deltas);
        let epoch = self.sealed_epoch();

        // Check every table, not just the ones this epoch touched: cheap at these sizes, and it
        // means a violation can never be missed because the guilty epoch touched another table.
        let names: Vec<String> = self.catalog.keys().cloned().collect();
        for name in names {
            self.contents_at(&name, epoch)?;
        }
        Ok(epoch)
    }

    /// A table's contents as of `epoch`, consolidated and sorted: the integral of its deltas over
    /// the log prefix `1..=epoch` (S-6).
    pub fn contents_at(&self, table: &str, epoch: Epoch) -> Result<Vec<(Row, i64)>> {
        if !self.catalog.contains_key(table) {
            return Err(OracleError::unknown_table(table));
        }
        if epoch > self.sealed_epoch() {
            return Err(OracleError::EpochOutOfRange {
                requested: epoch,
                sealed: self.sealed_epoch(),
            });
        }

        let mut integral: BTreeMap<Row, i64> = BTreeMap::new();
        let prefix = self
            .log
            .get(..epoch as usize)
            .ok_or(OracleError::EpochOutOfRange {
                requested: epoch,
                sealed: self.sealed_epoch(),
            })?;
        for deltas in prefix {
            for (row, weight) in deltas.entries_for(table) {
                let slot = integral.entry(row.clone()).or_insert(0);
                *slot =
                    slot.checked_add(*weight)
                        .ok_or(schweep_zset::ZSetError::WeightOverflow {
                            while_doing: "integrating a table's deltas",
                        })?;
            }
        }

        let mut out = Vec::with_capacity(integral.len());
        for (row, weight) in integral {
            if weight < 0 {
                return Err(OracleError::NegativeIntegral {
                    table: table.to_owned(),
                    row: row.to_string(),
                    weight,
                    epoch,
                });
            }
            if weight != 0 {
                out.push((row, weight));
            }
        }
        Ok(out)
    }

    /// A table's contents as a Z-set batch, for callers that want the Arrow form.
    pub fn contents_batch_at(&self, table: &str, epoch: Epoch) -> Result<ZSetBatch> {
        let schema = self
            .catalog
            .get(table)
            .ok_or_else(|| OracleError::unknown_table(table))?;
        Ok(ZSetBatch::from_entries(
            schema.clone(),
            self.contents_at(table, epoch)?,
        )?)
    }

    /// The answer to `query` as of the latest sealed epoch.
    pub fn answer(&self, query: &Query) -> Result<ZSetBatch> {
        self.answer_at(query, self.sealed_epoch())
    }

    /// The answer to `query` as of `epoch`: bind it, then recompute it from scratch (S-9).
    ///
    /// The stages run strictly in S-9 order — from, where, group, having, select — and each one
    /// consumes what the last produced.
    pub fn answer_at(&self, query: &Query, epoch: Epoch) -> Result<ZSetBatch> {
        if epoch > self.sealed_epoch() {
            return Err(OracleError::EpochOutOfRange {
                requested: epoch,
                sealed: self.sealed_epoch(),
            });
        }
        let bound = bind(query, &self.catalog)?;
        let mut live = LiveErrors::new();

        // FROM
        let mut relation = self.eval_source(&query.source, epoch)?;

        // WHERE (S-24: weights pass through untouched)
        if let Some(predicate) = &query.filter {
            let mut kept = Vec::new();
            for (row, weight) in relation.entries {
                // A row whose predicate raises has no truth value, so it is dropped and the error
                // is recorded as live (S-22a). It does not abort the recomputation: the complete
                // set of live errors has to be known before the least one can be reported (S-22c).
                match is_true(predicate, &row, &relation.schema) {
                    Ok(true) => kept.push((row, weight)),
                    Ok(false) => {}
                    Err(e) => live.record(e)?,
                }
            }
            relation = Relation {
                schema: relation.schema,
                entries: kept,
            };
        }

        // Consolidate before grouping. Aggregates need each distinct row once with its total
        // weight: an unconsolidated `(r, +1)` and `(r, -1)` would make MIN/MAX treat `r` as
        // present when it is not.
        relation = Relation {
            entries: consolidate(relation.entries)?,
            schema: relation.schema,
        };

        // GROUP BY, then HAVING
        if let Some(group_by) = &query.group_by {
            let grouped =
                self.eval_group_by(group_by, &relation, &bound.grouped_schema, &mut live)?;
            relation = grouped;
            if let Some(having) = &group_by.having {
                let mut kept = Vec::new();
                for (row, weight) in relation.entries {
                    match is_true(having, &row, &relation.schema) {
                        Ok(true) => kept.push((row, weight)),
                        Ok(false) => {}
                        Err(e) => live.record(e)?,
                    }
                }
                relation = Relation {
                    schema: relation.schema,
                    entries: kept,
                };
            }
        }

        // SELECT (S-25: weights preserved; distinct input rows may merge)
        if let Some(items) = &query.project {
            let mut projected = Vec::with_capacity(relation.entries.len());
            for (row, weight) in &relation.entries {
                let mut values = Vec::with_capacity(items.len());
                let mut raised = false;
                for item in items {
                    match eval(&item.value, row, &relation.schema) {
                        Ok(value) => values.push(value),
                        Err(e) => {
                            live.record(e)?;
                            raised = true;
                            break;
                        }
                    }
                }
                // A row missing a value in any output column cannot be emitted (S-22a).
                if !raised {
                    projected.push((Row::new(values), *weight));
                }
            }
            relation = Relation {
                schema: bound.output_schema.clone(),
                entries: projected,
            };
        }

        // The answer is an error while any live error is present, whatever else was computed
        // (S-22). The least message is reported (S-22c).
        if let Some(error) = live.least() {
            return Err(OracleError::Plan(error));
        }

        let mut entries = consolidate(relation.entries)?;

        // DISTINCT, last of all (S-34): every row present at all appears exactly once. Applied to
        // the consolidated answer, so "present" means total weight above zero.
        if query.distinct {
            for (_, weight) in &mut entries {
                // S-34 is defined on non-negative weights, and every stage from a table integral
                // onward preserves non-negativity. Checked rather than assumed: collapsing a
                // negative weight to 1 would invent a row.
                if *weight < 0 {
                    return Err(OracleError::NegativeIntermediate {
                        stage: "DISTINCT input",
                        weight: *weight,
                    });
                }
                *weight = 1;
            }
        }
        Ok(ZSetBatch::from_entries(bound.output_schema, entries)?)
    }

    /// The answer in canonical form (S-8) — what the differential harness compares (I-1).
    pub fn canonical_answer_at(&self, query: &Query, epoch: Epoch) -> Result<Canonical> {
        Ok(self.answer_at(query, epoch)?.canonical()?)
    }

    fn eval_source(&self, source: &Source, epoch: Epoch) -> Result<Relation> {
        match source {
            Source::Scan { table, alias } => {
                let schema = self
                    .catalog
                    .get(table)
                    .ok_or_else(|| OracleError::unknown_table(table.clone()))?;
                let qualified = Schema::new(
                    schema
                        .fields()
                        .iter()
                        .map(|f| {
                            schweep_zset::Field::new(
                                format!("{alias}.{}", f.name),
                                f.data_type,
                                f.nullable,
                            )
                        })
                        .collect(),
                )?;
                Ok(Relation {
                    schema: qualified,
                    entries: self.contents_at(table, epoch)?,
                })
            }

            // INNER equi-join by nested loop (S-26). Every pair is considered; a pair joins when
            // every key comparison is TRUE, which is why a null key never matches — `NULL = NULL`
            // is NULL, not true (S-13). The output weight is the product of the two input
            // weights, which is what makes multiplicities multiply.
            Source::Join { left, right, on } => {
                if on.is_empty() {
                    return Err(OracleError::Plan(
                        schweep_plan::PlanError::CrossJoinNotSupported,
                    ));
                }
                let l = self.eval_source(left, epoch)?;
                let r = self.eval_source(right, epoch)?;

                let mut key_indexes = Vec::with_capacity(on.len());
                for (lname, rname) in on {
                    let li = l.schema.index_of(lname).ok_or_else(|| {
                        OracleError::Plan(schweep_plan::PlanError::UnknownColumn {
                            name: lname.clone(),
                            scope: l.schema.to_string(),
                        })
                    })?;
                    let ri = r.schema.index_of(rname).ok_or_else(|| {
                        OracleError::Plan(schweep_plan::PlanError::UnknownColumn {
                            name: rname.clone(),
                            scope: r.schema.to_string(),
                        })
                    })?;
                    key_indexes.push((li, ri));
                }

                let mut fields = l.schema.fields().to_vec();
                fields.extend(r.schema.fields().iter().cloned());
                let schema = Schema::new(fields)?;

                let mut entries = Vec::new();
                for (lrow, lw) in &l.entries {
                    for (rrow, rw) in &r.entries {
                        if !keys_match(lrow, rrow, &key_indexes)? {
                            continue;
                        }
                        let weight = lw.checked_mul(*rw).ok_or(OracleError::JoinWeightOverflow)?;
                        let mut values = lrow.values().to_vec();
                        values.extend(rrow.values().iter().cloned());
                        entries.push((Row::new(values), weight));
                    }
                }
                Ok(Relation { schema, entries })
            }
        }
    }

    fn eval_group_by(
        &self,
        group_by: &schweep_plan::plan::GroupBy,
        input: &Relation,
        output_schema: &Schema,
        live: &mut LiveErrors,
    ) -> Result<Relation> {
        // Grouping treats nulls as equal to each other (S-28), which is *not* the three-valued
        // `=` of S-13. `Value`'s `Eq` says `Null == Null`, so a `BTreeMap` keyed by the key
        // values implements "not distinct from" exactly, and orders the groups deterministically
        // while it is at it (I-2).
        let mut groups: BTreeMap<Vec<Value>, Vec<(Row, i64)>> = BTreeMap::new();

        // The grand total is one group that exists whether or not any row does (S-33, D-20). Seeding
        // it here — with no members — is the whole implementation: every rule below then applies to it
        // unchanged, and S-30's "empty P" cases give COUNT 0 and the rest NULL.
        if group_by.keys.is_empty() {
            groups.insert(Vec::new(), Vec::new());
        }

        for (row, weight) in &input.entries {
            if *weight < 0 {
                return Err(OracleError::NegativeIntermediate {
                    stage: "GROUP BY input",
                    weight: *weight,
                });
            }
            let mut key = Vec::with_capacity(group_by.keys.len());
            let mut raised = false;
            for k in &group_by.keys {
                match eval(&k.value, row, &input.schema) {
                    Ok(value) => key.push(value),
                    Err(e) => {
                        live.record(e)?;
                        raised = true;
                        break;
                    }
                }
            }
            // A row with no key value belongs to no group, so it is dropped (S-22a).
            if raised {
                continue;
            }
            groups.entry(key).or_default().push((row.clone(), *weight));
        }

        let mut entries = Vec::with_capacity(groups.len());
        for (key, members) in groups {
            // A *keyed* group exists iff its total weight is positive; a drained one vanishes
            // rather than emitting a row of zeroes (S-29). A grand total has no key, so nothing for
            // its existence to depend on: it is always present (S-33, D-20).
            let mut total: i64 = 0;
            for (_, weight) in &members {
                total = total.checked_add(*weight).ok_or(OracleError::Plan(
                    schweep_plan::PlanError::AggregateOverflow { func: "GROUP BY" },
                ))?;
            }
            if total <= 0 && !group_by.keys.is_empty() {
                continue;
            }

            let mut values = key;
            let mut raised = false;
            for agg in &group_by.aggregates {
                match aggregate::evaluate(&agg.value, &members, &input.schema) {
                    Ok(value) => values.push(value),
                    Err(OracleError::Plan(e)) if e.is_evaluation_error() => {
                        live.record(e)?;
                        raised = true;
                        break;
                    }
                    Err(other) => return Err(other),
                }
            }
            // For an aggregate the unit is the group: a group whose aggregates cannot all be
            // evaluated produces no row at all (S-22a).
            if raised {
                continue;
            }
            // Each group produces exactly one row, at weight 1: an aggregate result is a
            // statement about a group, and a group has no multiplicity (S-27).
            entries.push((Row::new(values), 1));
        }

        Ok(Relation {
            schema: output_schema.clone(),
            entries,
        })
    }
}

fn keys_match(lrow: &Row, rrow: &Row, key_indexes: &[(usize, usize)]) -> Result<bool> {
    for (li, ri) in key_indexes {
        let lv = lrow.get(*li).ok_or(OracleError::NegativeIntermediate {
            stage: "join key lookup",
            weight: 0,
        })?;
        let rv = rrow.get(*ri).ok_or(OracleError::NegativeIntermediate {
            stage: "join key lookup",
            weight: 0,
        })?;
        // The comparison must be TRUE. A null on either side makes it NULL, so the pair does
        // not join (S-13, S-26).
        if lv.is_null() || rv.is_null() || lv != rv {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Merge equal rows by summing weights, drop zero weights, and sort — the same canonical form
/// [`schweep_zset::ZSetBatch::consolidate`] produces, computed on the row representation the
/// oracle works in.
fn consolidate(entries: Vec<(Row, i64)>) -> Result<Vec<(Row, i64)>> {
    let mut merged: BTreeMap<Row, i64> = BTreeMap::new();
    for (row, weight) in entries {
        let slot = merged.entry(row).or_insert(0);
        *slot = slot
            .checked_add(weight)
            .ok_or(schweep_zset::ZSetError::WeightOverflow {
                while_doing: "consolidating an intermediate relation",
            })?;
    }
    Ok(merged.into_iter().filter(|(_, w)| *w != 0).collect())
}
