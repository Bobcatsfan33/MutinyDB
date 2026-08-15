//! Incremental semantic operators inside MutinyDB's compute plane.
//!
//! PrismDB owns embedding-space identity and vector semantics. This crate owns the standing-state
//! consequence: each accepted row changes a generation-pinned ranking in `O(log n)`, while a
//! declared byte ceiling turns unbounded state into a named refusal.

use mutiny_bridge::BridgeDelta;
use prism_types::vector::dot;
use prism_types::{validate_and_normalize, Embedder, EmbeddingInput, EmbeddingPurpose};
use schweep_zset::{Row, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// A conservative default per-query state ceiling. Operators may be configured lower, never zero.
pub const DEFAULT_STATE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TOP_K: usize = 10_000;

/// Scalar columns fused with the semantic predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarColumns {
    pub tenant: String,
    pub event_time: i64,
    pub cost: f64,
    pub error: bool,
}

/// The supported M2 hybrid predicate. Every field is conjunctive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScalarPredicate {
    pub tenant: Option<String>,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,
    pub min_cost: Option<f64>,
    pub max_cost: Option<f64>,
    pub error: Option<bool>,
}

impl ScalarPredicate {
    #[must_use]
    pub fn admits(&self, columns: &ScalarColumns) -> bool {
        self.tenant.as_ref().is_none_or(|v| v == &columns.tenant)
            && self.time_from.is_none_or(|v| columns.event_time >= v)
            && self.time_to.is_none_or(|v| columns.event_time <= v)
            && self.min_cost.is_none_or(|v| columns.cost >= v)
            && self.max_cost.is_none_or(|v| columns.cost <= v)
            && self.error.is_none_or(|v| columns.error == v)
    }
}

/// One embedded row at the bridge boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticRecord {
    pub key: String,
    pub space: String,
    pub vector: Vec<f32>,
    pub columns: ScalarColumns,
}

impl SemanticRecord {
    pub fn new(
        key: impl Into<String>,
        space: impl Into<String>,
        mut vector: Vec<f32>,
        columns: ScalarColumns,
    ) -> Result<Self, SemanticError> {
        let key = key.into();
        let space = space.into();
        validate_name("record key", &key)?;
        validate_name("embedding space", &space)?;
        validate_columns(&columns)?;
        validate_and_normalize(&mut vector).map_err(|error| SemanticError::InvalidVector {
            reason: error.to_string(),
        })?;
        Ok(Self {
            key,
            space,
            vector,
            columns,
        })
    }
}

/// A Z-set change. M2 intentionally admits set weights only; multiplicities would make top-k row
/// identity ambiguous and are refused instead of silently collapsed.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticDelta {
    pub record: SemanticRecord,
    pub weight: i64,
}

/// A fixed standing top-k query.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticQuery {
    pub id: String,
    pub space: String,
    pub vector: Vec<f32>,
    pub k: usize,
    pub predicate: ScalarPredicate,
    pub state_budget_bytes: usize,
}

impl SemanticQuery {
    pub fn new(
        id: impl Into<String>,
        space: impl Into<String>,
        mut vector: Vec<f32>,
        k: usize,
        predicate: ScalarPredicate,
    ) -> Result<Self, SemanticError> {
        let id = id.into();
        let space = space.into();
        validate_name("query id", &id)?;
        validate_name("embedding space", &space)?;
        if k == 0 || k > MAX_TOP_K {
            return Err(SemanticError::InvalidQuery(format!(
                "k must be in 1..={MAX_TOP_K}"
            )));
        }
        validate_predicate(&predicate)?;
        validate_and_normalize(&mut vector).map_err(|error| SemanticError::InvalidVector {
            reason: error.to_string(),
        })?;
        Ok(Self {
            id,
            space,
            vector,
            k,
            predicate,
            state_budget_bytes: DEFAULT_STATE_BUDGET_BYTES,
        })
    }

    pub fn with_state_budget(mut self, bytes: usize) -> Result<Self, SemanticError> {
        if bytes == 0 {
            return Err(SemanticError::InvalidQuery(
                "state budget must be positive".to_owned(),
            ));
        }
        self.state_budget_bytes = bytes;
        Ok(self)
    }
}

/// One materialized answer row.
#[derive(Clone, Debug)]
pub struct SemanticHit {
    pub key: String,
    pub rank: usize,
    pub score: f32,
}

impl PartialEq for SemanticHit {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.rank == other.rank
            && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for SemanticHit {}

/// Retractions and insertions that move the standing answer from one epoch to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerDelta {
    pub hit: SemanticHit,
    pub weight: i64,
}

/// Generation-pinned exact top-k state.
#[derive(Clone, Debug)]
pub struct SemanticTopK {
    query: SemanticQuery,
    rows: BTreeMap<String, SemanticRecord>,
    ranking: BTreeSet<Ranked>,
    state_bytes: usize,
}

impl SemanticTopK {
    #[must_use]
    pub fn new(query: SemanticQuery) -> Self {
        Self {
            query,
            rows: BTreeMap::new(),
            ranking: BTreeSet::new(),
            state_bytes: 0,
        }
    }

    #[must_use]
    pub fn query(&self) -> &SemanticQuery {
        &self.query
    }

    #[must_use]
    pub fn state_bytes(&self) -> usize {
        self.state_bytes
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn answer(&self) -> Vec<SemanticHit> {
        self.ranking
            .iter()
            .take(self.query.k)
            .enumerate()
            .map(|(index, ranked)| SemanticHit {
                key: ranked.key.clone(),
                rank: index + 1,
                score: self
                    .rows
                    .get(&ranked.key)
                    .map_or(0.0, |record| dot(&self.query.vector, &record.vector)),
            })
            .collect()
    }

    /// Apply a whole epoch atomically. Any invalid row leaves state and answer unchanged.
    pub fn apply_epoch(
        &mut self,
        deltas: impl IntoIterator<Item = SemanticDelta>,
    ) -> Result<Vec<AnswerDelta>, SemanticError> {
        let before = self.answer();
        let mut next = self.clone();
        for delta in deltas {
            next.apply_one(delta)?;
        }
        if next.state_bytes > next.query.state_budget_bytes {
            return Err(SemanticError::StateBudgetExceeded {
                actual: next.state_bytes,
                limit: next.query.state_budget_bytes,
            });
        }
        let after = next.answer();
        *self = next;
        Ok(answer_diff(&before, &after))
    }

    fn apply_one(&mut self, delta: SemanticDelta) -> Result<(), SemanticError> {
        if delta.record.space != self.query.space {
            return Err(SemanticError::SpaceMismatch {
                query: self.query.space.clone(),
                row: delta.record.space,
            });
        }
        if delta.record.vector.len() != self.query.vector.len() {
            return Err(SemanticError::DimensionMismatch {
                expected: self.query.vector.len(),
                found: delta.record.vector.len(),
            });
        }
        match delta.weight {
            1 => self.insert(delta.record),
            -1 => self.remove(&delta.record),
            weight => Err(SemanticError::InvalidWeight(weight)),
        }
    }

    fn insert(&mut self, record: SemanticRecord) -> Result<(), SemanticError> {
        if self.rows.contains_key(&record.key) {
            return Err(SemanticError::DuplicateKey(record.key));
        }
        let bytes = record_bytes(&record);
        if self.query.predicate.admits(&record.columns) {
            self.ranking.insert(self.rank(&record));
        }
        self.state_bytes = self.state_bytes.saturating_add(bytes);
        self.rows.insert(record.key.clone(), record);
        Ok(())
    }

    fn remove(&mut self, record: &SemanticRecord) -> Result<(), SemanticError> {
        let held = self
            .rows
            .get(&record.key)
            .ok_or_else(|| SemanticError::UnknownRetraction(record.key.clone()))?;
        if !same_record(held, record) {
            return Err(SemanticError::RetractionMismatch(record.key.clone()));
        }
        let ranked = self.rank(held);
        let bytes = record_bytes(held);
        self.ranking.remove(&ranked);
        self.rows.remove(&record.key);
        self.state_bytes = self.state_bytes.saturating_sub(bytes);
        Ok(())
    }

    fn rank(&self, record: &SemanticRecord) -> Ranked {
        Ranked {
            score: Score(dot(&self.query.vector, &record.vector)),
            key: record.key.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Ranked {
    score: Score,
    key: String,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.key == other.key
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| self.key.cmp(&other.key))
    }
}

#[derive(Clone, Copy, Debug)]
struct Score(f32);

impl PartialEq for Score {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Score {}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher cosine first, matching PrismDB's exact rerank surface bit-for-bit.
        other.0.total_cmp(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("invalid {field}: it must be non-empty and contain no ASCII control character")]
    InvalidName { field: &'static str },
    #[error("invalid vector: {reason}")]
    InvalidVector { reason: String },
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    #[error("query is pinned to space {query:?}, but the row belongs to {row:?}")]
    SpaceMismatch { query: String, row: String },
    #[error("vector dimension mismatch: expected {expected}, found {found}")]
    DimensionMismatch { expected: usize, found: usize },
    #[error("semantic Z-set accepts only +1/-1 set weights, found {0}")]
    InvalidWeight(i64),
    #[error("record key {0:?} already exists; retract the old value before inserting an update")]
    DuplicateKey(String),
    #[error("cannot retract unknown record key {0:?}")]
    UnknownRetraction(String),
    #[error("retraction for key {0:?} does not exactly match the admitted record")]
    RetractionMismatch(String),
    #[error("semantic state requires {actual} bytes, over its declared {limit}-byte ceiling")]
    StateBudgetExceeded { actual: usize, limit: usize },
    #[error("bridge table mismatch: plan expects {expected:?}, delta names {found:?}")]
    BridgeTableMismatch { expected: String, found: String },
    #[error("bridge column {column} ({field}) is missing or has the wrong type")]
    BridgeColumn { column: usize, field: &'static str },
    #[error("embedding failed for row {key:?}: {reason}")]
    EmbeddingFailed { key: String, reason: String },
    #[error("semantic group configuration is invalid: {0}")]
    InvalidGrouping(String),
    #[error("semantic group merge overlaps record key {0:?}")]
    GroupMergeOverlap(String),
    #[error("migration epochs must contain the same keys and weights in both generations")]
    MigrationInputMismatch,
    #[error("migration cutover refused: {changed} of {compared} ranked positions changed, limit {limit}")]
    MigrationParity {
        changed: usize,
        compared: usize,
        limit: usize,
    },
    #[error("migration was already cut over")]
    MigrationAlreadyCutOver,
    #[error("one-shot source returned space {found:?}, expected {expected:?}")]
    OneShotSpaceMismatch { expected: String, found: String },
    #[error("one-shot source failed: {0}")]
    OneShot(String),
}

/// Fixed schema mapping from a bridged Schweep row into Prism's embedding boundary.
///
/// Cost is represented as integer micro-units because Schweep deliberately does not admit stored
/// floating-point columns. The mapping is configured once with the catalog, not guessed per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeColumns {
    pub key: usize,
    pub body: usize,
    pub tenant: usize,
    pub event_time: usize,
    pub cost_micros: usize,
    pub error: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeEmbeddingPlan {
    pub table: String,
    pub space: String,
    pub columns: BridgeColumns,
}

impl BridgeEmbeddingPlan {
    pub fn new(
        table: impl Into<String>,
        columns: BridgeColumns,
        embedder: &impl Embedder,
    ) -> Result<Self, SemanticError> {
        let table = table.into();
        validate_name("bridge table", &table)?;
        let space = embedding_space(embedder)?;
        Ok(Self {
            table,
            space,
            columns,
        })
    }

    /// Embed every row in one storage-commit-derived bridge batch. No partial result is returned:
    /// one malformed row or model refusal rejects the entire compute epoch.
    pub fn embed_delta(
        &self,
        delta: &BridgeDelta,
        embedder: &impl Embedder,
    ) -> Result<Vec<SemanticDelta>, SemanticError> {
        if delta.table != self.table {
            return Err(SemanticError::BridgeTableMismatch {
                expected: self.table.clone(),
                found: delta.table.clone(),
            });
        }
        let actual_space = embedding_space(embedder)?;
        if actual_space != self.space {
            return Err(SemanticError::SpaceMismatch {
                query: self.space.clone(),
                row: actual_space,
            });
        }
        self.embed_entries(&delta.entries, embedder)
    }

    /// The row-level half of [`Self::embed_delta`], exposed for catalog validation and fixtures.
    pub fn embed_entries(
        &self,
        entries: &[(Row, i64)],
        embedder: &impl Embedder,
    ) -> Result<Vec<SemanticDelta>, SemanticError> {
        let actual_space = embedding_space(embedder)?;
        if actual_space != self.space {
            return Err(SemanticError::SpaceMismatch {
                query: self.space.clone(),
                row: actual_space,
            });
        }
        entries
            .iter()
            .map(|(row, weight)| self.embed_row(row, *weight, embedder))
            .collect()
    }

    fn embed_row(
        &self,
        row: &Row,
        weight: i64,
        embedder: &impl Embedder,
    ) -> Result<SemanticDelta, SemanticError> {
        let key = required_str(row, self.columns.key, "key")?.to_owned();
        let body = required_str(row, self.columns.body, "body")?;
        let tenant = required_str(row, self.columns.tenant, "tenant")?.to_owned();
        let event_time = required_int(row, self.columns.event_time, "event_time")?;
        let cost_micros = required_int(row, self.columns.cost_micros, "cost_micros")?;
        if cost_micros < 0 {
            return Err(SemanticError::BridgeColumn {
                column: self.columns.cost_micros,
                field: "cost_micros",
            });
        }
        let error = required_bool(row, self.columns.error, "error")?;
        let vector = embedder
            .embed_scoped(EmbeddingInput {
                tenant_id: Some(&tenant),
                purpose: EmbeddingPurpose::Ingest,
                text: body,
            })
            .map_err(|error| SemanticError::EmbeddingFailed {
                key: key.clone(),
                reason: error.to_string(),
            })?;
        let record = SemanticRecord::new(
            key,
            self.space.clone(),
            vector,
            ScalarColumns {
                tenant,
                event_time,
                cost: cost_micros as f64 / 1_000_000.0,
                error,
            },
        )?;
        Ok(SemanticDelta { record, weight })
    }
}

fn embedding_space(embedder: &impl Embedder) -> Result<String, SemanticError> {
    let model = embedder.model_id();
    let version = embedder.model_version();
    validate_name("embedding model", model)?;
    validate_name("embedding version", version)?;
    Ok(format!("{model}:{version}"))
}

fn required_str<'a>(
    row: &'a Row,
    column: usize,
    field: &'static str,
) -> Result<&'a str, SemanticError> {
    match row.get(column) {
        Some(Value::Str(value)) => Ok(value),
        _ => Err(SemanticError::BridgeColumn { column, field }),
    }
}

fn required_int(row: &Row, column: usize, field: &'static str) -> Result<i64, SemanticError> {
    match row.get(column) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(SemanticError::BridgeColumn { column, field }),
    }
}

fn required_bool(row: &Row, column: usize, field: &'static str) -> Result<bool, SemanticError> {
    match row.get(column) {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(SemanticError::BridgeColumn { column, field }),
    }
}

/// Immutable, generation-pinned grouping anchors. Because anchors never drift inside an operator,
/// independently maintained disjoint partitions are exactly mergeable.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticGroupPlan {
    pub space: String,
    pub anchors: Vec<Vec<f32>>,
    pub predicate: ScalarPredicate,
    pub state_budget_bytes: usize,
}

impl SemanticGroupPlan {
    pub fn new(
        space: impl Into<String>,
        mut anchors: Vec<Vec<f32>>,
        predicate: ScalarPredicate,
    ) -> Result<Self, SemanticError> {
        let space = space.into();
        validate_name("embedding space", &space)?;
        validate_predicate(&predicate)?;
        let dimension = anchors.first().map(Vec::len).ok_or_else(|| {
            SemanticError::InvalidGrouping("at least one anchor is required".to_owned())
        })?;
        if dimension == 0 || anchors.iter().any(|anchor| anchor.len() != dimension) {
            return Err(SemanticError::InvalidGrouping(
                "anchors must be non-empty and have one dimension".to_owned(),
            ));
        }
        for anchor in &mut anchors {
            validate_and_normalize(anchor).map_err(|error| {
                SemanticError::InvalidGrouping(format!("invalid anchor: {error}"))
            })?;
        }
        Ok(Self {
            space,
            anchors,
            predicate,
            state_budget_bytes: DEFAULT_STATE_BUDGET_BYTES,
        })
    }

    pub fn with_state_budget(mut self, bytes: usize) -> Result<Self, SemanticError> {
        if bytes == 0 {
            return Err(SemanticError::InvalidGrouping(
                "state budget must be positive".to_owned(),
            ));
        }
        self.state_budget_bytes = bytes;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticGroupSummary {
    pub group_id: usize,
    pub count: usize,
    pub avg_cost: f64,
    pub error_rate: f64,
    pub exemplar_key: String,
    pub member_keys: Vec<String>,
}

/// Bounded incremental semantic grouping over fixed anchors.
#[derive(Clone, Debug)]
pub struct SemanticGroups {
    plan: SemanticGroupPlan,
    rows: BTreeMap<String, (SemanticRecord, usize)>,
    state_bytes: usize,
}

impl SemanticGroups {
    #[must_use]
    pub fn new(plan: SemanticGroupPlan) -> Self {
        Self {
            plan,
            rows: BTreeMap::new(),
            state_bytes: 0,
        }
    }

    #[must_use]
    pub fn plan(&self) -> &SemanticGroupPlan {
        &self.plan
    }

    #[must_use]
    pub fn state_bytes(&self) -> usize {
        self.state_bytes
    }

    pub fn apply_epoch(
        &mut self,
        deltas: impl IntoIterator<Item = SemanticDelta>,
    ) -> Result<(), SemanticError> {
        let mut next = self.clone();
        for delta in deltas {
            next.apply_one(delta)?;
        }
        if next.state_bytes > next.plan.state_budget_bytes {
            return Err(SemanticError::StateBudgetExceeded {
                actual: next.state_bytes,
                limit: next.plan.state_budget_bytes,
            });
        }
        *self = next;
        Ok(())
    }

    /// Merge a disjoint partition atomically. The full plan, including normalized anchors and
    /// scalar predicate, must match exactly; overlapping keys are a named refusal.
    pub fn merge_disjoint(&mut self, other: &Self) -> Result<(), SemanticError> {
        if self.plan != other.plan {
            return Err(SemanticError::InvalidGrouping(
                "cannot merge different spaces, anchors, predicates, or budgets".to_owned(),
            ));
        }
        if let Some(key) = other.rows.keys().find(|key| self.rows.contains_key(*key)) {
            return Err(SemanticError::GroupMergeOverlap(key.clone()));
        }
        self.apply_epoch(other.rows.values().map(|(record, _)| SemanticDelta {
            record: record.clone(),
            weight: 1,
        }))
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<SemanticGroupSummary> {
        let mut grouped: BTreeMap<usize, Vec<&SemanticRecord>> = BTreeMap::new();
        for (record, group) in self.rows.values() {
            if self.plan.predicate.admits(&record.columns) {
                grouped.entry(*group).or_default().push(record);
            }
        }
        grouped
            .into_iter()
            .filter_map(|(group_id, mut members)| {
                members.sort_by(|left, right| left.key.cmp(&right.key));
                let count = members.len();
                if count == 0 {
                    return None;
                }
                let cost = members
                    .iter()
                    .map(|record| record.columns.cost)
                    .sum::<f64>();
                let errors = members.iter().filter(|record| record.columns.error).count();
                let anchor = self.plan.anchors.get(group_id)?;
                let exemplar_key = members
                    .iter()
                    .min_by(|left, right| {
                        dot(anchor, &right.vector)
                            .total_cmp(&dot(anchor, &left.vector))
                            .then_with(|| left.key.cmp(&right.key))
                    })?
                    .key
                    .clone();
                Some(SemanticGroupSummary {
                    group_id,
                    count,
                    avg_cost: cost / count as f64,
                    error_rate: errors as f64 / count as f64,
                    exemplar_key,
                    member_keys: members.iter().map(|record| record.key.clone()).collect(),
                })
            })
            .collect()
    }

    fn apply_one(&mut self, delta: SemanticDelta) -> Result<(), SemanticError> {
        if delta.record.space != self.plan.space {
            return Err(SemanticError::SpaceMismatch {
                query: self.plan.space.clone(),
                row: delta.record.space,
            });
        }
        let dimension = self.plan.anchors.first().map_or(0, Vec::len);
        if delta.record.vector.len() != dimension {
            return Err(SemanticError::DimensionMismatch {
                expected: dimension,
                found: delta.record.vector.len(),
            });
        }
        match delta.weight {
            1 => {
                if self.rows.contains_key(&delta.record.key) {
                    return Err(SemanticError::DuplicateKey(delta.record.key));
                }
                let group = nearest_anchor(&self.plan.anchors, &delta.record.vector);
                self.state_bytes = self.state_bytes.saturating_add(record_bytes(&delta.record));
                self.rows
                    .insert(delta.record.key.clone(), (delta.record, group));
                Ok(())
            }
            -1 => {
                let (held, _) = self
                    .rows
                    .get(&delta.record.key)
                    .ok_or_else(|| SemanticError::UnknownRetraction(delta.record.key.clone()))?;
                if !same_record(held, &delta.record) {
                    return Err(SemanticError::RetractionMismatch(delta.record.key));
                }
                let bytes = record_bytes(held);
                self.rows.remove(&delta.record.key);
                self.state_bytes = self.state_bytes.saturating_sub(bytes);
                Ok(())
            }
            weight => Err(SemanticError::InvalidWeight(weight)),
        }
    }
}

fn nearest_anchor(anchors: &[Vec<f32>], vector: &[f32]) -> usize {
    anchors
        .iter()
        .enumerate()
        .max_by(|(left_index, left), (right_index, right)| {
            dot(left, vector)
                .total_cmp(&dot(right, vector))
                .then_with(|| right_index.cmp(left_index))
        })
        .map_or(0, |(index, _)| index)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    Mirroring,
    CutOver,
}

/// Two generation-pinned operator sets during a model migration. Inputs are admitted only in
/// paired epochs with identical logical identities; scores are never compared across spaces.
#[derive(Clone, Debug)]
pub struct GenerationMigration {
    primary: SemanticTopK,
    candidate: SemanticTopK,
    phase: MigrationPhase,
}

impl GenerationMigration {
    pub fn new(primary: SemanticTopK, candidate: SemanticTopK) -> Result<Self, SemanticError> {
        if primary.query.space == candidate.query.space {
            return Err(SemanticError::InvalidQuery(
                "migration requires two distinct embedding spaces".to_owned(),
            ));
        }
        if primary.query.id != candidate.query.id
            || primary.query.k != candidate.query.k
            || primary.query.predicate != candidate.query.predicate
        {
            return Err(SemanticError::InvalidQuery(
                "migration operators must share query identity, k, and scalar predicate".to_owned(),
            ));
        }
        Ok(Self {
            primary,
            candidate,
            phase: MigrationPhase::Mirroring,
        })
    }

    #[must_use]
    pub fn phase(&self) -> MigrationPhase {
        self.phase
    }

    #[must_use]
    pub fn answer(&self) -> Vec<SemanticHit> {
        match self.phase {
            MigrationPhase::Mirroring => self.primary.answer(),
            MigrationPhase::CutOver => self.candidate.answer(),
        }
    }

    #[must_use]
    pub fn candidate_answer(&self) -> Vec<SemanticHit> {
        self.candidate.answer()
    }

    pub fn apply_epoch(
        &mut self,
        primary: impl IntoIterator<Item = SemanticDelta>,
        candidate: impl IntoIterator<Item = SemanticDelta>,
    ) -> Result<(), SemanticError> {
        if self.phase == MigrationPhase::CutOver {
            return Err(SemanticError::MigrationAlreadyCutOver);
        }
        let primary = primary.into_iter().collect::<Vec<_>>();
        let candidate = candidate.into_iter().collect::<Vec<_>>();
        let primary_ids = primary
            .iter()
            .map(|delta| (&delta.record.key, delta.weight))
            .collect::<Vec<_>>();
        let candidate_ids = candidate
            .iter()
            .map(|delta| (&delta.record.key, delta.weight))
            .collect::<Vec<_>>();
        if primary_ids != candidate_ids {
            return Err(SemanticError::MigrationInputMismatch);
        }
        let mut next_primary = self.primary.clone();
        let mut next_candidate = self.candidate.clone();
        next_primary.apply_epoch(primary)?;
        next_candidate.apply_epoch(candidate)?;
        self.primary = next_primary;
        self.candidate = next_candidate;
        Ok(())
    }

    /// Cut over only after an explicit, reviewable rank-change allowance. This compares identities,
    /// not scores: scores belong to different geometries and are intentionally incomparable.
    pub fn cut_over(&mut self, allowed_rank_changes: usize) -> Result<(), SemanticError> {
        if self.phase == MigrationPhase::CutOver {
            return Err(SemanticError::MigrationAlreadyCutOver);
        }
        let primary = self.primary.answer();
        let candidate = self.candidate.answer();
        let compared = primary.len().max(candidate.len());
        let changed = (0..compared)
            .filter(|index| {
                primary.get(*index).map(|hit| &hit.key) != candidate.get(*index).map(|hit| &hit.key)
            })
            .count();
        if changed > allowed_rank_changes {
            return Err(SemanticError::MigrationParity {
                changed,
                compared,
                limit: allowed_rank_changes,
            });
        }
        self.phase = MigrationPhase::CutOver;
        Ok(())
    }
}

/// The cold-tier one-shot seam. Production adapters call PrismDB through its snapshot/object-store
/// path; tests can supply a deterministic oracle without adding a second ranking implementation.
pub trait OneShotSemanticSource {
    fn exact(&self, query: &SemanticQuery) -> Result<OneShotAnswer, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneShotAnswer {
    pub space: String,
    pub hits: Vec<SemanticHit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryRoute {
    Incremental,
    ColdOneShot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedAnswer {
    pub route: QueryRoute,
    pub space: String,
    pub hits: Vec<SemanticHit>,
}

/// Route bounded hot state locally and overflowed/archival state through Prism's cold exact path.
/// Both paths return the generation alongside the result so callers cannot silently fuse them.
pub fn route_exact(
    standing: &SemanticTopK,
    max_hot_state_bytes: usize,
    cold: &impl OneShotSemanticSource,
) -> Result<RoutedAnswer, SemanticError> {
    if max_hot_state_bytes == 0 {
        return Err(SemanticError::InvalidQuery(
            "hot-route budget must be positive".to_owned(),
        ));
    }
    if standing.state_bytes <= max_hot_state_bytes {
        return Ok(RoutedAnswer {
            route: QueryRoute::Incremental,
            space: standing.query.space.clone(),
            hits: standing.answer(),
        });
    }
    let answer = cold
        .exact(&standing.query)
        .map_err(SemanticError::OneShot)?;
    if answer.space != standing.query.space {
        return Err(SemanticError::OneShotSpaceMismatch {
            expected: standing.query.space.clone(),
            found: answer.space,
        });
    }
    validate_one_shot_hits(&answer.hits, standing.query.k)?;
    Ok(RoutedAnswer {
        route: QueryRoute::ColdOneShot,
        space: standing.query.space.clone(),
        hits: answer.hits,
    })
}

fn validate_one_shot_hits(hits: &[SemanticHit], k: usize) -> Result<(), SemanticError> {
    if hits.len() > k
        || hits.iter().enumerate().any(|(index, hit)| {
            hit.rank != index + 1 || !hit.score.is_finite() || hit.key.trim().is_empty()
        })
    {
        return Err(SemanticError::OneShot(
            "source returned an invalid rank sequence, score, key, or result bound".to_owned(),
        ));
    }
    let unique = hits.iter().map(|hit| &hit.key).collect::<BTreeSet<_>>();
    if unique.len() != hits.len() {
        return Err(SemanticError::OneShot(
            "source returned duplicate keys".to_owned(),
        ));
    }
    Ok(())
}

fn answer_diff(before: &[SemanticHit], after: &[SemanticHit]) -> Vec<AnswerDelta> {
    let before_set = before.to_vec();
    let after_set = after.to_vec();
    let mut output = Vec::new();
    for hit in &before_set {
        if !after_set.contains(hit) {
            output.push(AnswerDelta {
                hit: hit.clone(),
                weight: -1,
            });
        }
    }
    for hit in &after_set {
        if !before_set.contains(hit) {
            output.push(AnswerDelta {
                hit: hit.clone(),
                weight: 1,
            });
        }
    }
    output
}

fn validate_name(field: &'static str, value: &str) -> Result<(), SemanticError> {
    if value.trim().is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        Err(SemanticError::InvalidName { field })
    } else {
        Ok(())
    }
}

fn validate_columns(columns: &ScalarColumns) -> Result<(), SemanticError> {
    validate_name("tenant", &columns.tenant)?;
    if !columns.cost.is_finite() || columns.cost < 0.0 {
        return Err(SemanticError::InvalidQuery(
            "row cost must be finite and non-negative".to_owned(),
        ));
    }
    Ok(())
}

fn validate_predicate(predicate: &ScalarPredicate) -> Result<(), SemanticError> {
    for value in [predicate.min_cost, predicate.max_cost]
        .into_iter()
        .flatten()
    {
        if !value.is_finite() || value < 0.0 {
            return Err(SemanticError::InvalidQuery(
                "cost bounds must be finite and non-negative".to_owned(),
            ));
        }
    }
    if predicate
        .min_cost
        .zip(predicate.max_cost)
        .is_some_and(|(a, b)| a > b)
    {
        return Err(SemanticError::InvalidQuery(
            "minimum cost exceeds maximum cost".to_owned(),
        ));
    }
    if predicate
        .time_from
        .zip(predicate.time_to)
        .is_some_and(|(a, b)| a > b)
    {
        return Err(SemanticError::InvalidQuery(
            "time lower bound exceeds upper bound".to_owned(),
        ));
    }
    Ok(())
}

fn record_bytes(record: &SemanticRecord) -> usize {
    record.key.len()
        + record.space.len()
        + record.columns.tenant.len()
        + record.vector.len().saturating_mul(std::mem::size_of::<f32>())
        + std::mem::size_of::<ScalarColumns>()
        // Conservative ownership/index allowance: one key/value B-tree node and, for admitted
        // rows, one ranking node. Exact allocator accounting is platform-specific; undercounting
        // is not acceptable at an admission boundary, so both are charged for every row.
        + 256
}

fn same_record(left: &SemanticRecord, right: &SemanticRecord) -> bool {
    left.key == right.key
        && left.space == right.space
        && left.columns == right.columns
        && left.vector.len() == right.vector.len()
        && left
            .vector
            .iter()
            .zip(&right.vector)
            .all(|(a, b)| a.to_bits() == b.to_bits())
}
