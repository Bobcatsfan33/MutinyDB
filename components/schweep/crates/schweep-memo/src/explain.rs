//! `EXPLAIN STATE`: what every operator is holding, per query (`ARCHITECTURE.md` §6 C8, I-9).
//!
//! I-9 says every stateful operator declares its state bound and the runtime accounts actual state
//! against it. Through C7 that accounting existed but was *internal*: `Circuit::check_state_declarations`
//! enforced it and `state_fingerprint` printed it inside a wall of operator contents. Nobody running a
//! query could ask what it was costing.
//!
//! This is that question, answered per query and per operator:
//!
//! ```text
//!   EXPLAIN STATE
//!   backends: RedbBackend (spilled to /var/current/state)
//!
//!   query 0
//!     node 2   aggregate  entries      41984  budget       262144  private   [1 + 2x ...]
//!     total                            41984 entries
//!
//!   query 1
//!     node 2   aggregate  entries      41984  budget       262144  shared with 1 other
//!     node 3   distinct   entries       1024  budget        65536  private
//!     total                            43008 entries
//!
//!   dataflow: 43008 entries in 3 backend(s), counted once each
//!   bytes: between 5234688 and 16932096 (measured envelope)
//! ```
//!
//! ## The number that is reported, and the number that is true
//!
//! **Entries are the measurement; bytes are a measured *envelope*.** The frozen `StateBackend` trait
//! accounts in entries (D-15), because an entry count is a fact every backend can state exactly and a
//! byte count is not: redb's file grows in regions, holds a header, and reclaims lazily. Worse, the
//! per-entry cost depends on how wide the key is — measured at 67…200 bytes across two key widths — and
//! the frozen trait cannot be asked how wide its keys are without materialising them, which is exactly
//! what state larger than RAM forbids.
//!
//! So the byte column is a floor and a ceiling, both measured ([`crate::costs`]). That is weaker than a
//! point estimate and much more honest than one, because [`Reconciliation`] can then assert something
//! true: **the actual footprint lies inside the envelope**. A reported number nobody checks is
//! decoration, and this crate has a standing rule about those.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schweep_circuit::NodeId;

use crate::registry::{Handle, Memo};

/// The **measured envelope** `EXPLAIN STATE` reports bytes with.
///
/// Every constant is ledgered (`testing/evidence/registry.json`) against
/// `testing/evidence/c8-state-costs.json`. It is an envelope rather than a point estimate because a
/// per-entry cost depends on key width and the frozen trait reports entries, not bytes — see
/// [`crate::costs`] for the measurements and the argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// Every entry costs at least this much. A **bound**, measured.
    pub bytes_per_entry_low: u64,
    /// The largest per-entry cost measured. Reported, **not** a bound — key width is unbounded.
    pub bytes_per_entry_typical_max: u64,
    pub bytes_per_backend: u64,
    /// Whether the backends this model describes occupy files at all.
    pub on_disk: bool,
}

impl CostModel {
    /// The measured model for `RedbBackend`.
    #[must_use]
    pub const fn redb() -> CostModel {
        CostModel {
            bytes_per_entry_low: crate::costs::BYTES_PER_ENTRY_LOW,
            bytes_per_entry_typical_max: crate::costs::BYTES_PER_ENTRY_TYPICAL_MAX,
            bytes_per_backend: crate::costs::BYTES_PER_BACKEND,
            on_disk: true,
        }
    }

    /// The model for an in-memory backend: no file, and a figure nothing can falsify.
    #[must_use]
    pub const fn memory() -> CostModel {
        CostModel {
            bytes_per_entry_low: crate::costs::BYTES_PER_ENTRY_IN_MEMORY,
            bytes_per_entry_typical_max: crate::costs::BYTES_PER_ENTRY_IN_MEMORY,
            bytes_per_backend: 0,
            on_disk: false,
        }
    }

    /// The **floor**: bytes this many entries must occupy at least.
    ///
    /// No per-backend overhead is added: a redb file preallocates and then *truncates on commit*, so a
    /// backend holding a few thousand entries can occupy **less** than an empty one — measured, in
    /// `testing/evidence/c8-state-costs.json`. A floor that charged the preallocation would claim a
    /// minimum the store routinely goes below.
    #[must_use]
    pub fn floor(&self, entries: u64) -> u64 {
        entries * self.bytes_per_entry_low
    }

    /// What an *empty* set of backends may occupy: their preallocated files and nothing more.
    #[must_use]
    pub fn empty_allowance(&self, backends: u64) -> u64 {
        backends * self.bytes_per_backend
    }

    /// A typical-case figure, for the report's benefit. **Not a bound** — see
    /// [`crate::costs::BYTES_PER_ENTRY_TYPICAL_MAX`].
    #[must_use]
    pub fn typical(&self, entries: u64) -> u64 {
        entries * self.bytes_per_entry_typical_max
    }
}

/// One operator's state, as reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorState {
    pub node: NodeId,
    pub operator: &'static str,
    /// The I-9 declaration, rendered.
    pub declared: String,
    /// Entries held — **measured**, from the backend itself.
    pub entries: usize,
    /// The I-9 budget this operator's declaration allows, or `None` for an admitted-unbounded one.
    pub budget: Option<usize>,
    /// How many other registered queries read this node.
    pub shared_with: usize,
    /// How many backends this operator was handed (a join has two).
    pub backends: usize,
}

/// One query's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryState {
    pub handle: Handle,
    pub operators: Vec<OperatorState>,
}

impl QueryState {
    /// Entries this query's operators hold, **counting a shared operator once**.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.operators.iter().map(|op| op.entries).sum()
    }

    #[must_use]
    pub fn backends(&self) -> usize {
        self.operators.iter().map(|op| op.backends).sum()
    }
}

/// The whole report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainState {
    pub backends: String,
    pub queries: Vec<QueryState>,
    /// Entries across the dataflow, **counting each operator once however many queries read it**.
    ///
    /// The sum of the per-query totals double-counts shared operators, which is the right number for
    /// "what does this query cost me" and the wrong one for "what is the process holding". Both are
    /// reported, and which is which is stated, because a memo whose whole purpose is sharing must not
    /// report sharing twice and call it usage.
    pub distinct_entries: usize,
    pub distinct_backends: usize,
    pub model: CostModel,
}

impl ExplainState {
    /// The floor the dataflow's footprint cannot be below, and a typical-case figure beside it.
    ///
    /// The first is a bound; the second is not, and [`ExplainState::render`] says so where a reader
    /// will see it.
    #[must_use]
    pub fn byte_floor_and_typical(&self) -> (u64, u64) {
        (
            self.model.floor(self.distinct_entries as u64),
            self.model.typical(self.distinct_entries as u64),
        )
    }

    /// The report, as text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("EXPLAIN STATE\nbackends: {}\n", self.backends);
        for query in &self.queries {
            if query.handle == Handle::unregistered() {
                let _ = write!(out, "\ncircuit (no registration)\n");
            } else {
                let _ = write!(out, "\nquery {}\n", query.handle.id());
            }
            for op in &query.operators {
                let budget = match op.budget {
                    Some(budget) => budget.to_string(),
                    None => "admitted-unbounded".to_owned(),
                };
                let shared = if op.shared_with > 0 {
                    format!("shared with {} other quer(y|ies)", op.shared_with)
                } else {
                    "private".to_owned()
                };
                let _ = writeln!(
                    out,
                    "  node {:<3} {:<10} entries {:>10}  budget {:>12}  {}  [{}]",
                    op.node.index(),
                    op.operator,
                    op.entries,
                    budget,
                    shared,
                    op.declared
                );
            }
            let _ = writeln!(out, "  total{:>24}{:>10} entries", "", query.entries());
        }
        let (floor, typical) = self.byte_floor_and_typical();
        let _ = writeln!(
            out,
            "\ndataflow: {} entries in {} backend(s), counted once each\n\
             bytes: at least {floor} (a measured bound); typically around {typical} for keys of \
             ordinary width — NOT a bound, because key width is unbounded (schweep_memo::costs)",
            self.distinct_entries, self.distinct_backends
        );
        out
    }
}

/// What the reconciliation gate checks: the report against the backends themselves.
///
/// **Three claims, and the third is the one that took two attempts to get right.**
///
/// 1. **The count.** The entries the report attributes to the queries must equal the entries the
///    operators actually hold, counted by a *different* walk: the report walks registrations and their
///    nodes; this walks the dataflow's live nodes and asks each operator. Two paths to one truth, so a
///    report that mangles the number in either direction disagrees with it.
/// 2. **The floor.** With `n` entries, the backends must occupy at least `n × BYTES_PER_ENTRY_LOW` on
///    disk. This catches an operator reporting state it does not have.
/// 3. **Presence.** No entries means no footprint beyond the empty files.
///
/// There is deliberately **no upper bound on bytes per entry**, because there is no upper bound on key
/// width: an operator's key carries the row's values, and a row may hold a string of any length. An
/// earlier version asserted a byte *envelope* with a ceiling, and the C8 ceiling gate broke it with a
/// 480-character padding column — the fix was to stop claiming what cannot be true, and to add claim 1,
/// which does not need a byte model at all. Without it a report that *under*-stated its entries passed:
/// under-reporting only lowers the floor, and a lower floor is easier to clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconciliation {
    pub reported_entries: usize,
    pub reported_backends: usize,
    /// Entries the operators actually hold, counted independently of the report.
    pub actual_entries: usize,
    /// The measured floor: bytes the reported entries must occupy at least.
    pub floor: u64,
    /// What an empty set of these backends would be allowed to occupy.
    pub empty_allowance: u64,
    pub actual_bytes: u64,
    /// False for an in-memory backend, where there is nothing on disk to reconcile against.
    pub reconcilable: bool,
}

impl Reconciliation {
    /// Do the report and the backends agree, on every claim that can be checked?
    ///
    /// The count is checked on **both** backends — it needs no bytes — and the two byte claims only
    /// where there is a disk to look at.
    #[must_use]
    pub fn agrees(&self) -> bool {
        if self.reported_entries != self.actual_entries {
            return false;
        }
        if !self.reconcilable {
            return true;
        }
        if self.reported_entries == 0 {
            return self.actual_bytes <= self.empty_allowance;
        }
        self.actual_bytes >= self.floor
    }

    /// How many bytes the store actually spends per reported entry.
    ///
    /// Reported rather than bounded. A reader watching this number drift far from the measured
    /// 67…205 for ordinary keys is watching either wide rows or a bug, and the report should let them
    /// tell which.
    #[must_use]
    pub fn bytes_per_entry(&self) -> Option<f64> {
        if !self.reconcilable || self.reported_entries == 0 {
            return None;
        }
        Some(self.actual_bytes as f64 / self.reported_entries as f64)
    }

    #[must_use]
    pub fn render(&self) -> String {
        if !self.reconcilable {
            return format!(
                "{} entries in {} backend(s) · nothing on disk to reconcile (in-memory)",
                self.reported_entries, self.reported_backends
            );
        }
        let per_entry = self
            .bytes_per_entry()
            .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.0}"));
        format!(
            "{} entries reported / {} held in {} backend(s) · floor {} · actual {} · \
             {per_entry} bytes/entry · {}",
            self.reported_entries,
            self.actual_entries,
            self.reported_backends,
            self.floor,
            self.actual_bytes,
            if self.agrees() { "agrees" } else { "DISAGREES" }
        )
    }
}

/// Reconcile a circuit's report against the circuit itself (the soak gate's door).
pub fn reconcile_circuit(
    circuit: &schweep_circuit::Circuit,
    model: CostModel,
    backends: String,
    actual_bytes: u64,
) -> crate::Result<Reconciliation> {
    let report = explain_circuit(circuit, model, backends)?;
    Ok(Reconciliation {
        reported_entries: report.distinct_entries,
        reported_backends: report.distinct_backends,
        actual_entries: circuit.total_state_size(),
        floor: model.floor(report.distinct_entries as u64),
        empty_allowance: model.empty_allowance(report.distinct_backends as u64),
        actual_bytes,
        reconcilable: model.on_disk,
    })
}

/// `EXPLAIN STATE` for a single circuit, with no registry above it.
///
/// The memo's version below is the one a client would reach. This one exists because a `Memo` keeps the
/// accumulated input in memory for C7's mid-history catch-up, so a memo cannot itself run under a
/// memory ceiling its *data* exceeds — and the C8 ceiling gate is about operator state, not about that
/// cache. The gate therefore drives a circuit directly and reports through here.
///
/// That limitation is named in `docs/PROGRESS.md` rather than worked around: sourcing catch-up from the
/// log and the snapshot instead of from RAM is C9's, where the server owns both.
pub fn explain_circuit(
    circuit: &schweep_circuit::Circuit,
    model: CostModel,
    backends: String,
) -> crate::Result<ExplainState> {
    let mut operators = Vec::new();
    let mut distinct_entries = 0usize;
    let mut distinct_backends = 0usize;
    for index in 0..circuit.node_count() {
        let node = NodeId::from(index);
        let Some(report) = circuit.node_state(node)? else {
            continue;
        };
        distinct_entries += report.entries;
        distinct_backends += report.backends;
        operators.push(OperatorState {
            node,
            operator: report.operator,
            declared: report.declared,
            entries: report.entries,
            budget: report.budget,
            shared_with: 0,
            backends: report.backends,
        });
    }
    Ok(ExplainState {
        backends,
        queries: vec![QueryState {
            handle: Handle::unregistered(),
            operators,
        }],
        distinct_entries,
        distinct_backends,
        model,
    })
}

impl Memo {
    /// `EXPLAIN STATE` for every registered query.
    pub fn explain_state(&self, model: CostModel) -> crate::Result<ExplainState> {
        // How many *other* registrations read each node — the sharing column.
        let mut readers: BTreeMap<NodeId, usize> = BTreeMap::new();
        for registration in self.registrations().values() {
            for node in &registration.nodes {
                *readers.entry(*node).or_insert(0) += 1;
            }
        }

        let mut queries = Vec::new();
        let mut distinct: BTreeMap<NodeId, (usize, usize)> = BTreeMap::new();
        for registration in self.registrations().values() {
            let mut operators = Vec::new();
            for node in &registration.nodes {
                let Some(report) = self.dataflow().node_state(*node)? else {
                    // A source holds nothing, so it is not part of a state report.
                    continue;
                };
                distinct.insert(*node, (report.entries, report.backends));
                operators.push(OperatorState {
                    node: *node,
                    operator: report.operator,
                    declared: report.declared,
                    entries: report.entries,
                    budget: report.budget,
                    shared_with: readers.get(node).copied().unwrap_or(1).saturating_sub(1),
                    backends: report.backends,
                });
            }
            queries.push(QueryState {
                handle: registration.handle,
                operators,
            });
        }

        Ok(ExplainState {
            backends: self.backends().describe(),
            queries,
            distinct_entries: distinct.values().map(|(entries, _)| entries).sum(),
            distinct_backends: distinct.values().map(|(_, backends)| backends).sum(),
            model,
        })
    }

    /// Reconcile the report against what the backends actually occupy.
    pub fn reconcile(&self, model: CostModel, actual_bytes: u64) -> crate::Result<Reconciliation> {
        let report = self.explain_state(model)?;
        Ok(Reconciliation {
            reported_entries: report.distinct_entries,
            reported_backends: report.distinct_backends,
            // The independent count: the dataflow's own walk of its live operators, which does not go
            // through the report at all.
            actual_entries: self.dataflow().total_state_size(),
            floor: model.floor(report.distinct_entries as u64),
            empty_allowance: model.empty_allowance(report.distinct_backends as u64),
            actual_bytes,
            reconcilable: model.on_disk,
        })
    }
}
