//! Incremental semantic operators inside MutinyDB's compute plane.
//!
//! PrismDB owns embedding-space identity and vector semantics. This crate owns the standing-state
//! consequence: each accepted row changes a generation-pinned ranking in `O(log n)`, while a
//! declared byte ceiling turns unbounded state into a named refusal.

use prism_types::validate_and_normalize;
use prism_types::vector::dot;
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
